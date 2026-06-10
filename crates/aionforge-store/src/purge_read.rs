//! The erasure-cascade closure walk (05 §3, M5.T03) — the read half of the one
//! destructive path in the system.
//!
//! `derived_from_closure` computes, **read-only against one snapshot**, the exact set of
//! nodes a hard purge of the seed would destroy: the transitive closure over *incoming*
//! `DERIVED_FROM` edges (the edge points derivative → source, so derivatives of a node
//! are found on its incoming side), plus each doomed node's exclusively-owned
//! `ProvenanceRecord`. Nothing here writes; the write half (the purge primitive)
//! consumes the computed closure in a single transaction, so an over-large or refused
//! cascade never opens a write transaction at all.
//!
//! **The multi-parent survival rule.** A derivative joins the closure only when *every*
//! one of its sources is itself in the closure. A deduplicated fact derived from two
//! episodes survives the erasure of one of them — deleting it would silently destroy a
//! memory still grounded in a source the caller never asked to erase. Spared
//! derivatives are reported, not dropped silently.
//!
//! Because admission depends on set membership and discovery order is not topological,
//! the walk runs to a **fixed point**: a candidate spared early is re-evaluated as the
//! doomed set grows (its last surviving sibling source may join later). The doomed set
//! only grows, so the iteration is monotone and terminates. A visited discipline (the
//! doomed and pending sets) guards against malformed `DERIVED_FROM` cycles — the edge is
//! polymorphic `any → any`, so a cycle is type-possible even though derivation is
//! logically a DAG.
//!
//! `MENTIONS`, `ABOUT`, `SUPPORTS`, and every other edge are never traversed: a shared
//! entity referenced by surviving memories must survive, and attestation/audit linkage
//! is severed by the node deletion itself, never used to find more victims.

use std::collections::{HashMap, HashSet, VecDeque};

use aionforge_domain::edges::{DerivedFrom, HasProvenance};
use aionforge_domain::ids::Id;
use selene_core::db_string;

use crate::NodeId;
use crate::convert::as_id;
use crate::error::StoreError;
use crate::store::Store;

/// Hard bounds on one cascade. Exceeding either is a typed refusal decided during the
/// read phase — never a partial or truncated purge. The values are policy, supplied by
/// the orchestrator; the store enforces whatever it is handed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CascadeCaps {
    /// The deepest derivation level the closure may reach (the seed is depth 0).
    pub max_depth: usize,
    /// The most nodes the closure may contain, provenance records included.
    pub max_nodes: usize,
}

/// The computed erasure set for one seed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeClosure {
    /// Every node the purge will destroy, in admission order: the seed first, then
    /// derivatives as the fixed point admits them, then provenance records.
    pub nodes: Vec<NodeId>,
    /// The domain ids of `nodes`, index-parallel — the id-only spine the purge audit
    /// and report carry (never content).
    pub node_ids: Vec<Id>,
    /// The deepest derivation level admitted (the seed is 0).
    pub cascade_depth: usize,
    /// Derivatives evaluated and **spared** by the multi-parent survival rule: still
    /// grounded in at least one source outside the closure.
    pub spared_multiparent: Vec<Id>,
    /// How many of `nodes` are `ProvenanceRecord` additions. Exclusivity holds by the
    /// data model (one record per write, keyed to a single subject), not by a check
    /// here — the walk follows the one outgoing `HAS_PROVENANCE` hop and trusts it.
    pub provenance_count: usize,
}

/// The outcome of a closure computation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClosureOutcome {
    /// The full closure, within caps.
    Computed(PurgeClosure),
    /// The cascade exceeded a cap. Nothing was computed beyond the observation and no
    /// write can follow; the caller refuses the whole erasure.
    TooLarge {
        /// Doomed nodes observed when the cap fired.
        nodes_observed: usize,
        /// The derivation depth observed when the cap fired.
        depth_observed: usize,
    },
    /// The seed is not a live node — already purged, or never resolved. A typed
    /// outcome, not an error: the orchestrator pre-resolves the seed, so reaching this
    /// is a benign race with a concurrent deletion, and the caller answers it the same
    /// way it answers an unresolvable id.
    SeedNotLive,
}

impl Store {
    /// Compute the erasure closure for `seed` (05 §3, M5.T03): the fixed-point
    /// transitive closure over incoming `DERIVED_FROM` under the multi-parent survival
    /// rule, plus each doomed node's `ProvenanceRecord`, bounded by `caps`.
    ///
    /// Read-only: one snapshot, no locks held against writers, nothing mutated. A seed
    /// that is not a live node is the typed [`ClosureOutcome::SeedNotLive`], not an
    /// error.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a read or decode fails.
    pub fn derived_from_closure(
        &self,
        seed: NodeId,
        caps: &CascadeCaps,
    ) -> Result<ClosureOutcome, StoreError> {
        let snapshot = self.graph().read();
        let derived = db_string(DerivedFrom::LABEL)?;
        let provenance = db_string(HasProvenance::LABEL)?;
        let id_key = db_string("id")?;

        if snapshot.node_properties(seed).is_none() {
            return Ok(ClosureOutcome::SeedNotLive);
        }

        let mut doomed: Vec<NodeId> = vec![seed];
        let mut doomed_set: HashSet<NodeId> = HashSet::from([seed]);
        let mut depth_of: HashMap<NodeId, usize> = HashMap::from([(seed, 0)]);
        let mut deepest = 0usize;
        let mut pending: Vec<NodeId> = Vec::new();
        let mut pending_set: HashSet<NodeId> = HashSet::new();
        let mut queue: VecDeque<NodeId> = VecDeque::from([seed]);

        // Every outgoing DERIVED_FROM source of `node` already doomed?
        let all_sources_doomed =
            |node: NodeId, doomed_set: &HashSet<NodeId>| -> Result<bool, StoreError> {
                let Some(adjacency) = snapshot.outgoing_edges(node) else {
                    return Ok(true);
                };
                for edge in adjacency.iter_label(&derived) {
                    if !doomed_set.contains(&edge.neighbor) {
                        return Ok(false);
                    }
                }
                Ok(true)
            };

        loop {
            // Discover derivatives of everything newly doomed.
            while let Some(node) = queue.pop_front() {
                let Some(adjacency) = snapshot.incoming_edges(node) else {
                    continue;
                };
                for edge in adjacency.iter_label(&derived) {
                    let candidate = edge.neighbor;
                    if doomed_set.contains(&candidate) || pending_set.contains(&candidate) {
                        continue;
                    }
                    if all_sources_doomed(candidate, &doomed_set)? {
                        // Depth: one past the deepest source — every source is doomed
                        // (and so depth-mapped) by the admission rule, so this is
                        // well-defined regardless of which edge discovered the node.
                        let depth = 1 + source_depth(&snapshot, candidate, &derived, &depth_of)?;
                        if depth > caps.max_depth || doomed.len() + 1 > caps.max_nodes {
                            return Ok(ClosureOutcome::TooLarge {
                                nodes_observed: doomed.len() + 1,
                                depth_observed: depth.max(deepest),
                            });
                        }
                        doomed.push(candidate);
                        doomed_set.insert(candidate);
                        depth_of.insert(candidate, depth);
                        deepest = deepest.max(depth);
                        queue.push_back(candidate);
                    } else {
                        pending.push(candidate);
                        pending_set.insert(candidate);
                    }
                }
            }

            // The fixed-point sweep: a pending candidate whose last surviving source
            // just entered the set becomes doomed now and re-enters discovery.
            let mut progressed = false;
            let mut still_pending = Vec::with_capacity(pending.len());
            for candidate in pending.drain(..) {
                if all_sources_doomed(candidate, &doomed_set)? {
                    // Depth: one past its deepest doomed source.
                    let depth = 1 + source_depth(&snapshot, candidate, &derived, &depth_of)?;
                    if depth > caps.max_depth || doomed.len() + 1 > caps.max_nodes {
                        return Ok(ClosureOutcome::TooLarge {
                            nodes_observed: doomed.len() + 1,
                            depth_observed: depth.max(deepest),
                        });
                    }
                    pending_set.remove(&candidate);
                    doomed.push(candidate);
                    doomed_set.insert(candidate);
                    depth_of.insert(candidate, depth);
                    deepest = deepest.max(depth);
                    queue.push_back(candidate);
                    progressed = true;
                } else {
                    still_pending.push(candidate);
                }
            }
            pending = still_pending;
            if !progressed && queue.is_empty() {
                break;
            }
        }

        // Each doomed node's exclusively-owned provenance record joins the closure —
        // an orphaned ProvenanceRecord carries content about the erased memory and
        // would survive as exactly the shadow record erasure exists to remove.
        let mut provenance_count = 0usize;
        for index in 0..doomed.len() {
            let node = doomed[index];
            let Some(adjacency) = snapshot.outgoing_edges(node) else {
                continue;
            };
            for edge in adjacency.iter_label(&provenance) {
                if doomed_set.insert(edge.neighbor) {
                    if doomed.len() + 1 > caps.max_nodes {
                        return Ok(ClosureOutcome::TooLarge {
                            nodes_observed: doomed.len() + 1,
                            depth_observed: deepest,
                        });
                    }
                    doomed.push(edge.neighbor);
                    provenance_count += 1;
                }
            }
        }

        // The id-only spine: every closure and spared node resolves its domain id.
        let id_of = |node: NodeId| -> Result<Id, StoreError> {
            let props = snapshot.node_properties(node).ok_or_else(|| {
                StoreError::invariant("closure member has no properties".to_string())
            })?;
            let value = props.get(&id_key).ok_or_else(|| {
                StoreError::decode("closure member missing required property `id`".to_string())
            })?;
            as_id(value)
        };
        let node_ids = doomed.iter().map(|n| id_of(*n)).collect::<Result<_, _>>()?;
        let spared_multiparent = pending
            .iter()
            .map(|n| id_of(*n))
            .collect::<Result<_, _>>()?;

        Ok(ClosureOutcome::Computed(PurgeClosure {
            nodes: doomed,
            node_ids,
            cascade_depth: deepest,
            spared_multiparent,
            provenance_count,
        }))
    }
}

/// The deepest already-doomed source of `node`, for depth accounting on a fixed-point
/// admission (every source is doomed when this is called).
fn source_depth(
    snapshot: &selene_graph::SeleneGraph,
    node: NodeId,
    derived: &selene_core::DbString,
    depth_of: &HashMap<NodeId, usize>,
) -> Result<usize, StoreError> {
    let Some(adjacency) = snapshot.outgoing_edges(node) else {
        return Ok(0);
    };
    Ok(adjacency
        .iter_label(derived)
        .filter_map(|edge| depth_of.get(&edge.neighbor).copied())
        .max()
        .unwrap_or(0))
}
