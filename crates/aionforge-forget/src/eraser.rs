//! The right-to-erasure orchestrator (05 §3, M5.T03) — the layer that turns one seed id
//! into one irreversible, audited, fully-reported cascade.
//!
//! `Eraser::erase` runs the two store halves in order: the read-only closure walk (the
//! fixed-point `DERIVED_FROM` cascade under the multi-parent survival rule, bounded by
//! the policy caps) and the single-transaction hard purge. Everything refusable is
//! refused **before** the write half: an unresolvable seed, a dead seed, and an
//! over-cap cascade all return without opening a write transaction.
//!
//! The eraser deliberately consults **none** of the forgetter's protections. Pinned,
//! attested, high-importance, referenced — every gate that spares a memory from the
//! sweep is an eligibility rule for the *reversible* path; erasure is the explicit,
//! principal-driven escalation those gates defer to (the forgetter's attested refusal
//! literally names this cascade as its owner). Erase succeeds on a pinned, attested
//! memory by design.
//!
//! What gates it instead is the **namespace authority** (06 §1): the caller supplies
//! the acting [`Principal`] and an [`Authorizer`], and the eraser demands write-grade
//! authority over *every* namespace the computed closure spans. One refused namespace
//! refuses the whole erasure — never a partial purge of the authorized subset, which
//! would tear a derivation chain in half and leave derivatives grounded in nothing.
//! The check runs after the walk (the span is only known once the closure is) and
//! before the audit and purge, so an unauthorized erase touches nothing and writes
//! nothing. The purge audit names the principal as its actor: erasure is the one
//! agent-driven write on the forgetting side, so pinning the row to the substrate
//! actor would hide exactly the accountability the audit exists to provide.
//!
//! What the cascade does not follow, it names: a purged node's `PROMOTED_TO` global
//! copy lives in another namespace other agents depend on, so the core path stops at
//! the namespace boundary and reports the survivor in
//! [`EraseReport::promoted_shadows`] — erasing it too is the owner-gated follow slice.
//! The report also states where erased content still physically resides
//! ([`ResidualRetention`]): the dead rows and vector tombstones until
//! [`Store::compact`], and the WAL — which today has no scheduled eviction, because
//! the store does not yet drive the substrate's snapshot pipeline. Honest reporting
//! over comfortable silence.

use std::sync::Arc;

use aionforge_domain::authz::{Authorizer, Principal};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::Timestamp;
use aionforge_store::{CascadeCaps, ClosureOutcome, PurgeClosure, PurgeWrite, Store, StoreError};

use crate::audit_addr::{namespace_identity, transition_id};
use crate::forgetter::ALL_MEMORY_LABELS;
use crate::policy::ErasurePolicy;

/// Where erased content still physically resides after a successful erase. Both flags
/// are true today; they exist so the surface can flip honestly as the reclaim and
/// snapshot wiring land, instead of the report ever overclaiming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResidualRetention {
    /// Dead row slots and vector-index tombstones remain in the live graph until the
    /// next [`Store::compact`] pass physically reclaims them.
    pub live_until_compact: bool,
    /// Pre-purge property values remain in the WAL. The substrate's snapshot
    /// publication truncates the log when it runs, but the store does not yet drive
    /// that pipeline — until it does, this residue has no scheduled eviction.
    pub wal_archive_until_snapshot: bool,
}

/// What one erase destroyed, spared, and left behind — the id-only spine of the
/// cascade, never content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EraseReport {
    /// The seed the caller named.
    pub seed: Id,
    /// Nodes destroyed, the seed and provenance records included.
    pub purged_nodes: usize,
    /// The domain ids of everything destroyed — the demonstrable record.
    pub purged_node_ids: Vec<Id>,
    /// Distinct edges severed, every label, both directions.
    pub purged_edges: usize,
    /// The deepest derivation level the cascade reached (the seed is 0).
    pub cascade_depth: usize,
    /// How many of the purged nodes were exclusively-owned provenance records.
    pub purged_provenance: usize,
    /// Derivatives spared by the multi-parent survival rule — still grounded in a
    /// surviving source the caller never asked to erase.
    pub spared_multiparent: Vec<Id>,
    /// Cross-namespace `PROMOTED_TO` copies of purged nodes that this erase left
    /// alive: named, not followed (the owner-gated follow slice owns that boundary).
    pub promoted_shadows: Vec<Id>,
    /// Where erased content still physically resides.
    pub residual_retention: ResidualRetention,
    /// The id of the `Purge` audit row this erase co-committed.
    pub purge_audit_id: Id,
}

/// The outcome of a point erase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointErase {
    /// The cascade was destroyed and audited; the report is the receipt.
    Erased(EraseReport),
    /// No live memory carries this id — never resolved, or already purged.
    NotFound,
    /// The cascade exceeded a policy cap and the whole erasure was refused before any
    /// write; nothing changed.
    CascadeTooLarge {
        /// Doomed nodes observed when the cap fired.
        nodes_observed: usize,
        /// The derivation depth observed when the cap fired.
        depth_observed: usize,
    },
    /// The principal lacks write authority over a namespace the cascade spans. The
    /// whole erasure was refused before any write — never a partial purge of the
    /// authorized subset — and nothing changed.
    Unauthorized {
        /// The first spanned namespace the authority refused.
        namespace: Namespace,
    },
    /// Erasure is not enabled; nothing was read or written. The honest answer to a
    /// host calling a switched-off surface — never a fabricated "not found".
    Disabled,
}

/// The right-to-erasure orchestrator. Held by the engine as an `Option` — absent means
/// off, and every erase surface is inert.
pub struct Eraser {
    store: Arc<Store>,
    policy: ErasurePolicy,
}

impl Eraser {
    /// Build over the store with a validated policy.
    #[must_use]
    pub fn new(store: Arc<Store>, policy: ErasurePolicy) -> Self {
        Self { store, policy }
    }

    /// The policy this eraser runs.
    #[must_use]
    pub fn policy(&self) -> &ErasurePolicy {
        &self.policy
    }

    /// Erase one memory and its derivation cascade by id (05 §3): irreversible,
    /// audited, fully reported. No eligibility gate — the forgetter's protections
    /// spare from the *reversible* sweep; this is the explicit escalation they defer
    /// to, and it succeeds on a pinned or attested memory by design. What does gate it
    /// is `authorizer`: the principal must hold write authority over every namespace
    /// the closure spans, or the whole erasure is the typed
    /// [`PointErase::Unauthorized`] refusal and nothing is touched.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a read, walk, or write fails. Every refusal is a
    /// typed [`PointErase`] outcome, decided before the write transaction opens.
    pub fn erase(
        &self,
        principal: &Principal,
        authorizer: &dyn Authorizer,
        id: &Id,
        now: &Timestamp,
    ) -> Result<PointErase, StoreError> {
        let Some(candidate) = self.store.memory_by_id(id, &ALL_MEMORY_LABELS)? else {
            return Ok(PointErase::NotFound);
        };
        let caps = CascadeCaps {
            max_depth: self.policy.max_cascade_depth,
            max_nodes: self.policy.max_cascade_nodes,
        };
        let closure = match self.store.derived_from_closure(candidate.node, &caps)? {
            ClosureOutcome::Computed(closure) => closure,
            ClosureOutcome::TooLarge {
                nodes_observed,
                depth_observed,
            } => {
                return Ok(PointErase::CascadeTooLarge {
                    nodes_observed,
                    depth_observed,
                });
            }
            // The seed died between resolution and the walk: the memory is gone,
            // which is the outcome the caller asked for someone else to have caused.
            ClosureOutcome::SeedNotLive => return Ok(PointErase::NotFound),
        };
        // The authority rules on every namespace the cascade spans, seed's own first
        // (encounter order). One refusal refuses the whole erasure, before the shadow
        // scan, the audit, and the purge — an unauthorized erase reads, but never
        // writes.
        for namespace in &closure.namespaces {
            if authorizer.authorize_write(principal, namespace).is_err() {
                return Ok(PointErase::Unauthorized {
                    namespace: namespace.clone(),
                });
            }
        }
        let promoted_shadows = self.store.promoted_targets(&closure.nodes)?;

        let audit = purge_audit(
            id,
            &principal.agent_id,
            &candidate.identity.namespace,
            &closure,
            &promoted_shadows,
            now,
        );
        let purge_audit_id = audit.identity.id;
        match self.store.hard_purge(&closure.nodes, &audit)? {
            PurgeWrite::Applied {
                deleted_nodes,
                deleted_edges,
            } => Ok(PointErase::Erased(EraseReport {
                seed: *id,
                purged_nodes: deleted_nodes,
                purged_node_ids: closure.node_ids,
                purged_edges: deleted_edges,
                cascade_depth: closure.cascade_depth,
                purged_provenance: closure.provenance_count,
                spared_multiparent: closure.spared_multiparent,
                promoted_shadows,
                residual_retention: ResidualRetention {
                    live_until_compact: true,
                    wal_archive_until_snapshot: true,
                },
                purge_audit_id,
            })),
            // Everything died between the walk and the write — same answer as a dead
            // seed: the memory is gone, nothing here destroyed it.
            PurgeWrite::Noop => Ok(PointErase::NotFound),
        }
    }
}

/// The purge audit event: one fresh row per applied erase, in the seed memory's own
/// namespace, naming the erasing principal as actor, with an id-and-scalar payload —
/// counts and a reason, never content.
fn purge_audit(
    seed: &Id,
    actor: &Id,
    namespace: &Namespace,
    closure: &PurgeClosure,
    promoted_shadows: &[Id],
    now: &Timestamp,
) -> AuditEvent {
    AuditEvent {
        identity: namespace_identity(transition_id(), namespace.clone(), now),
        kind: AuditKind::Purge,
        subject_id: *seed,
        actor_id: *actor,
        payload: serde_json::json!({
            "reason": "right_to_erasure",
            "cascade_count": closure.nodes.len(),
            "cascade_depth": closure.cascade_depth,
            "provenance_count": closure.provenance_count,
            "spared_multiparent": closure.spared_multiparent.len(),
            "promoted_shadows": promoted_shadows.len(),
        }),
        signature: String::new(),
        occurred_at: now.clone(),
    }
}
