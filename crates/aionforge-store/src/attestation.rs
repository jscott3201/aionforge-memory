//! The attestation write surface and its readers (06 §4).
//!
//! A signed attestation is a `Fact -ATTESTED_BY-> Agent` edge plus an `attest` audit. The
//! edge is **immutable** (`attested_at`/`signature` are `IMMUTABLE` in the catalog), so it
//! is written only when absent — a re-attestation by the same agent is a no-op, never a
//! mutation. This is a split-out `impl Store` (one impl may span modules in a crate); it
//! reaches the private graph through the `pub(crate)` [`Store::graph`] accessor.

use aionforge_domain::edges::{AttestedBy, Audit};
use aionforge_domain::ids::Id;
use aionforge_domain::nodes::forensic::AuditEvent;
use selene_core::{NodeId, PropertyMap, db_string};
use selene_graph::{RowIndex, SeleneGraph};

use crate::convert::{as_id, as_str, id_value, key, string_value, timestamp_value};
use crate::error::StoreError;
use crate::store::Store;
use crate::{audit, materialize};

const ID: &str = "id";
const ATTESTED_AT: &str = "attested_at";
const SIGNATURE: &str = "signature";
const CATEGORY: &str = "category";

/// One distinct attester of a fact: the attesting agent's domain id and the trust category
/// its attestation was made under (if any). The orchestrator weights each attester by its
/// reliability in this category.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttesterRecord {
    /// The attesting agent's domain id.
    pub attester_id: Id,
    /// The category the attestation applies to, or `None` for an uncategorized one.
    pub category: Option<String>,
}

/// The node ids touched by an [`Store::attest_fact`] write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttestWriteIds {
    /// The `attest` audit event (existing on a replay, freshly written otherwise).
    pub audit: NodeId,
}

/// The `ATTESTED_BY` edge property map (immutable signature + attest instant + optional
/// category).
fn attested_by_props(edge: &AttestedBy) -> Result<PropertyMap, StoreError> {
    let mut pairs = vec![
        (key(ATTESTED_AT)?, timestamp_value(&edge.attested_at)),
        (key(SIGNATURE)?, string_value(&edge.signature)?),
    ];
    if let Some(category) = &edge.category {
        pairs.push((key(CATEGORY)?, string_value(category)?));
    }
    Ok(PropertyMap::from_pairs(pairs)?)
}

impl Store {
    /// Record a signed attestation of a fact, atomically (06 §4).
    ///
    /// One commit writes `fact -ATTESTED_BY-> attester` **only when absent** — the edge is
    /// immutable, so a repeat by the same attester never mutates the recorded
    /// signature/instant — plus the content-addressed `attest` [`AuditEvent`] and its
    /// `AUDIT` edge to the fact, deduped so a replay writes no second audit. Durable before
    /// visible; nothing is published if any step fails.
    ///
    /// # Errors
    /// Returns [`StoreError`] if translating the edge or audit, any mutation, or the commit
    /// fails.
    pub fn attest_fact(
        &self,
        fact: NodeId,
        attester: NodeId,
        edge: &AttestedBy,
        audit: &AuditEvent,
    ) -> Result<AttestWriteIds, StoreError> {
        let edge_props = attested_by_props(edge)?;
        let audit_edge = db_string(Audit::LABEL)?;

        let mut txn = self.graph().begin_write();
        let ids = {
            let mut mutator = txn.mutator();
            // Write-when-absent: ensure_edge skips when the edge already exists, so the
            // immutable ATTESTED_BY signature/instant is never rewritten (06 §4).
            materialize::ensure_edge(&mut mutator, AttestedBy::LABEL, fact, attester, edge_props)?;
            // Content-addressed dedup against the in-txn working graph (committed state plus
            // this txn's writes), under the write lock — so a concurrent re-attest cannot probe
            // the same id and both create a second audit. Mirrors the consolidation audit dedup.
            let ensured = audit::ensure_event(&mut mutator, audit)?;
            if ensured.created {
                mutator.create_edge(
                    audit_edge,
                    ensured.node,
                    fact,
                    PropertyMap::from_pairs(Vec::new())?,
                )?;
            }
            AttestWriteIds {
                audit: ensured.node,
            }
        };
        txn.commit()?;
        Ok(ids)
    }

    /// The distinct attesters of a fact, one per attesting agent (06 §4).
    ///
    /// Reads the fact's outgoing `ATTESTED_BY` edges, resolving each to its `Agent` neighbor's
    /// domain id and the edge's category. Distinct by agent id — one agent's vote counts once
    /// even in the (edge-prevented) event of a duplicate edge — so the count is the independent
    /// quorum size.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a neighbor node or edge cannot be decoded.
    pub fn distinct_attesters(&self, fact: NodeId) -> Result<Vec<AttesterRecord>, StoreError> {
        let snapshot = self.graph().read();
        let label = db_string(AttestedBy::LABEL)?;
        let Some(adjacency) = snapshot.outgoing_edges(fact) else {
            return Ok(Vec::new());
        };
        let id_key = db_string(ID)?;
        let category_key = db_string(CATEGORY)?;
        let mut seen: Vec<Id> = Vec::new();
        let mut records: Vec<AttesterRecord> = Vec::new();
        for edge in adjacency.iter_label(&label) {
            let Some(props) = snapshot.node_properties(edge.neighbor) else {
                continue;
            };
            let Some(id_value) = props.get(&id_key) else {
                continue;
            };
            let attester_id = as_id(id_value)?;
            if seen.contains(&attester_id) {
                continue;
            }
            seen.push(attester_id);
            let category = snapshot
                .edge_properties(edge.edge_id)
                .and_then(|edge_props| edge_props.get(&category_key).cloned())
                .map(|value| as_str(&value).map(str::to_string))
                .transpose()?;
            records.push(AttesterRecord {
                attester_id,
                category,
            });
        }
        Ok(records)
    }

    /// The `NodeId` of the fact with this domain id, if one exists — live or expired.
    ///
    /// A probe over the `Fact.id` scalar index (registered for exactly this; the index lets the
    /// probe mean anything — `nodes_with_property_eq` reads as absent without one). Quorum
    /// promotion uses it two ways: to resolve a candidate fact id the attester named, and to
    /// check whether a promoted global copy already exists (the idempotency probe in
    /// [`Store::promote_fact`]). `Fact.id` is `UNIQUE`, so at most one node matches.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the id cannot be encoded for the lookup.
    pub fn fact_node_by_id(&self, id: &Id) -> Result<Option<NodeId>, StoreError> {
        fact_node_in(&self.graph().read(), id)
    }

    /// The `NodeId` of the registered agent with this domain id, if one exists.
    ///
    /// A probe over the `Agent.id` index. Quorum promotion needs the attester's node id to wire
    /// the `Fact -ATTESTED_BY-> Agent` edge — the gate resolves the agent's *key* by id, this
    /// resolves its *node*. `Agent.id` is `UNIQUE`, so at most one node matches.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the id cannot be encoded for the lookup.
    pub fn agent_node_by_id(&self, id: &Id) -> Result<Option<NodeId>, StoreError> {
        node_by_id_in(&self.graph().read(), "Agent", id)
    }
}

/// Resolve a `Fact`'s `NodeId` by its domain id within a given snapshot — the committed graph
/// for [`Store::fact_node_by_id`], or a transaction's working graph (`mutator.read()`) for the
/// in-txn idempotency probe in [`Store::promote_fact`], so the probe and its write share one
/// view under the write lock.
pub(crate) fn fact_node_in(snapshot: &SeleneGraph, id: &Id) -> Result<Option<NodeId>, StoreError> {
    node_by_id_in(snapshot, "Fact", id)
}

/// Resolve an `Agent`'s `NodeId` by its domain id within a snapshot — the in-txn twin of
/// [`Store::agent_node_by_id`], so a trust-fold write can probe and wire its `AUDIT` edge to the
/// agent under one write lock.
pub(crate) fn agent_node_in(snapshot: &SeleneGraph, id: &Id) -> Result<Option<NodeId>, StoreError> {
    node_by_id_in(snapshot, "Agent", id)
}

/// Resolve a node's `NodeId` by its domain `id` within a snapshot, for a label whose `id` is
/// scalar-indexed and `UNIQUE`. Returns `None` when no index is registered (read as absent) or
/// no live row matches.
fn node_by_id_in(
    snapshot: &SeleneGraph,
    label: &str,
    id: &Id,
) -> Result<Option<NodeId>, StoreError> {
    let label = db_string(label)?;
    let prop = db_string(ID)?;
    let value = id_value(id)?;
    let Some(rows) = snapshot.nodes_with_property_eq(&label, &prop, &value) else {
        return Ok(None);
    };
    for row in rows.iter() {
        let Some(node) = snapshot.node_id_for_row(RowIndex::new(row)) else {
            continue;
        };
        if snapshot.node_properties(node).is_some() {
            return Ok(Some(node));
        }
    }
    Ok(None)
}
