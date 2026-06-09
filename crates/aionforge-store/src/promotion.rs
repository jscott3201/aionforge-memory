//! The quorum-promotion write surface and the `Promotion` ledger translation (06 §4).
//!
//! Promotion copies a team fact into the `global` namespace as a new, content-addressed
//! node, links the original to the copy with `PROMOTED_TO`, and records the decision in a
//! `Promotion` ledger row. Demotion is the reversible inverse: it links the copy back with
//! `DEMOTED_FROM`, quarantines the copy (`expired_at = now`), and flips the ledger to
//! `rejected` — and it never touches the namespace original. Both write-sets are one atomic
//! commit and are idempotent: a replay (or a crash mid-write) converges to the same graph,
//! keyed on the content-addressed global-fact id, the candidate's single ledger row, and
//! the content-addressed audit ids.
//!
//! A split-out `impl Store` plus the `Promotion` node translation, mirroring [`crate::audit`].

use aionforge_domain::blocks::Identity;
use aionforge_domain::edges::{About, Audit, DemotedFrom, PromotedTo};
use aionforge_domain::ids::Id;
use aionforge_domain::nodes::forensic::{AuditEvent, Promotion};
use aionforge_domain::nodes::semantic::{Fact, FactStatus};
use aionforge_domain::time::Timestamp;
use selene_core::{
    DbString, LabelDiff, LabelSet, NodeId, PropertyDiff, PropertyMap, Value, db_string,
};
use selene_graph::{RowIndex, SeleneGraph};

use crate::convert::{
    as_f64, as_id, as_namespace, as_timestamp, as_u64, enum_from_value, enum_value, id_value, key,
    namespace_value, timestamp_value,
};
use crate::error::StoreError;
use crate::store::Store;
use crate::{audit, fact, materialize};

const ID: &str = "id";
const INGESTED_AT: &str = "ingested_at";
const NAMESPACE: &str = "namespace";
const EXPIRED_AT: &str = "expired_at";
const CANDIDATE_FACT_ID: &str = "candidate_fact_id";
const POSTERIOR: &str = "posterior";
const K: &str = "k";
const STATUS: &str = "status";
const RESOLVED_AT: &str = "resolved_at";
const PROMOTED_FACT_ID: &str = "promoted_fact_id";

/// The selene-db node label for a promotion ledger row.
pub(crate) fn label() -> Result<LabelSet, StoreError> {
    Ok(LabelSet::single(db_string(Promotion::LABEL)?))
}

/// Translate a [`Promotion`] into `(labels, properties)` for `create_node`.
pub(crate) fn to_node(ledger: &Promotion) -> Result<(LabelSet, PropertyMap), StoreError> {
    let mut pairs: Vec<(DbString, Value)> = Vec::with_capacity(10);
    pairs.push((key(ID)?, id_value(&ledger.identity.id)?));
    pairs.push((
        key(INGESTED_AT)?,
        timestamp_value(&ledger.identity.ingested_at),
    ));
    pairs.push((
        key(NAMESPACE)?,
        namespace_value(&ledger.identity.namespace)?,
    ));
    if let Some(expired_at) = &ledger.identity.expired_at {
        pairs.push((key(EXPIRED_AT)?, timestamp_value(expired_at)));
    }
    pairs.push((
        key(CANDIDATE_FACT_ID)?,
        id_value(&ledger.candidate_fact_id)?,
    ));
    pairs.push((key(POSTERIOR)?, Value::Float(ledger.posterior)));
    pairs.push((key(K)?, Value::Uint(ledger.k)));
    pairs.push((key(STATUS)?, enum_value(&ledger.status)?));
    if let Some(resolved_at) = &ledger.resolved_at {
        pairs.push((key(RESOLVED_AT)?, timestamp_value(resolved_at)));
    }
    if let Some(promoted_fact_id) = &ledger.promoted_fact_id {
        pairs.push((key(PROMOTED_FACT_ID)?, id_value(promoted_fact_id)?));
    }
    Ok((label()?, PropertyMap::from_pairs(pairs)?))
}

/// Reconstruct a [`Promotion`] from a node's stored property map.
pub(crate) fn from_properties(props: &PropertyMap) -> Result<Promotion, StoreError> {
    let get =
        |name: &str| -> Result<Option<&Value>, StoreError> { Ok(props.get(&db_string(name)?)) };
    let require = |name: &str| -> Result<&Value, StoreError> {
        get(name)?.ok_or_else(|| StoreError::decode(format!("missing required property `{name}`")))
    };
    Ok(Promotion {
        identity: Identity {
            id: as_id(require(ID)?)?,
            ingested_at: as_timestamp(require(INGESTED_AT)?)?,
            namespace: as_namespace(require(NAMESPACE)?)?,
            expired_at: get(EXPIRED_AT)?.map(as_timestamp).transpose()?,
        },
        candidate_fact_id: as_id(require(CANDIDATE_FACT_ID)?)?,
        posterior: as_f64(require(POSTERIOR)?)?,
        k: as_u64(require(K)?)?,
        status: enum_from_value(require(STATUS)?)?,
        resolved_at: get(RESOLVED_AT)?.map(as_timestamp).transpose()?,
        promoted_fact_id: get(PROMOTED_FACT_ID)?.map(as_id).transpose()?,
    })
}

/// Find the single promotion ledger row for a candidate fact, by the indexed
/// `candidate_fact_id`. The ledger is content-addressed one-per-candidate, so at most one
/// row matches.
fn find_by_candidate(snapshot: &SeleneGraph, candidate: &Id) -> Result<Option<NodeId>, StoreError> {
    let label = db_string(Promotion::LABEL)?;
    let prop = db_string(CANDIDATE_FACT_ID)?;
    let value = id_value(candidate)?;
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

/// The bi-temporal property map shared by `PROMOTED_TO` and `DEMOTED_FROM`.
fn lineage_props(temporal: &aionforge_domain::time::BiTemporal) -> Result<PropertyMap, StoreError> {
    Ok(PropertyMap::from_pairs(fact::bitemporal_pairs(temporal))?)
}

/// The mutable resolution fields of a ledger row (status, posterior, k, and — when set —
/// the resolution instant and promoted-fact id). Used to update an existing row in place
/// to its terminal state without recreating it.
fn resolution_diff(ledger: &Promotion) -> Result<PropertyDiff, StoreError> {
    let mut set: Vec<(DbString, Value)> = vec![
        (key(STATUS)?, enum_value(&ledger.status)?),
        (key(POSTERIOR)?, Value::Float(ledger.posterior)),
        (key(K)?, Value::Uint(ledger.k)),
    ];
    if let Some(resolved_at) = &ledger.resolved_at {
        set.push((key(RESOLVED_AT)?, timestamp_value(resolved_at)));
    }
    if let Some(promoted_fact_id) = &ledger.promoted_fact_id {
        set.push((key(PROMOTED_FACT_ID)?, id_value(promoted_fact_id)?));
    }
    Ok(PropertyDiff::new(set, [])?)
}

/// The node ids touched by a [`Store::promote_fact`] write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromoteWriteIds {
    /// The promoted global-namespace fact (existing on a replay, freshly written otherwise).
    pub global_fact: NodeId,
    /// The `promote` audit event.
    pub audit: NodeId,
}

impl Store {
    /// The single promotion ledger row for a candidate fact, if one exists (06 §4).
    ///
    /// A targeted by-candidate read over the indexed `candidate_fact_id`. There is **no**
    /// status-scan reader: an attester must name a candidate, never browse pending ones (06 §4).
    ///
    /// # Errors
    /// Returns [`StoreError`] if the row cannot be decoded into a [`Promotion`].
    pub fn promotion_by_candidate(&self, candidate: &Id) -> Result<Option<Promotion>, StoreError> {
        let snapshot = self.graph().read();
        match find_by_candidate(&snapshot, candidate)? {
            Some(node) => Ok(snapshot
                .node_properties(node)
                .map(from_properties)
                .transpose()?),
            None => Ok(None),
        }
    }

    /// Promote a team fact to `global`, atomically and idempotently (06 §4).
    ///
    /// One commit writes the global-namespace copy (a new, content-addressed `Fact` node)
    /// with its `ABOUT` edge to the same canonical subject entity the team fact points at,
    /// the `team -PROMOTED_TO-> global` lineage edge, the `Promotion` ledger row (status
    /// `promoted`), and the `promote` audit with its `AUDIT` edge to the team fact. The team
    /// original is never touched — promotion is additive, so an "as of" view of the team
    /// namespace is unchanged.
    ///
    /// Idempotent: the global copy's id is content-addressed, so a replay finds the existing
    /// node (the `Fact.id` probe) and writes no second one; the lineage and `ABOUT` edges are
    /// written only when absent (healing a crash between the node and its edges); the ledger
    /// is created or updated in place; the audit is content-addressed and deduped.
    ///
    /// # Errors
    /// Returns [`StoreError::Invariant`] if the `ABOUT` or `PROMOTED_TO` window is out of
    /// order, or if the team fact has no `ABOUT` edge; [`StoreError`] if a translation,
    /// mutation, or the commit fails. Nothing is published if any step fails.
    pub fn promote_fact(
        &self,
        team_fact: NodeId,
        global_fact: &Fact,
        about: &About,
        promoted: &PromotedTo,
        ledger: &Promotion,
        audit: &AuditEvent,
    ) -> Result<PromoteWriteIds, StoreError> {
        if !about.temporal.windows_ordered() {
            return Err(StoreError::invariant(
                "promoted fact ABOUT window bounds are out of order".to_string(),
            ));
        }
        if !promoted.temporal.windows_ordered() {
            return Err(StoreError::invariant(
                "PROMOTED_TO window bounds are out of order".to_string(),
            ));
        }

        // Resolve the canonical subject entity from the team fact's ABOUT edge — the global
        // copy points at the same shared entity node. The team fact is not modified here, so
        // this committed read is stable for the write below.
        let subject_entity = self.about_neighbor(team_fact)?.ok_or_else(|| {
            StoreError::decode("promoted team fact has no ABOUT edge".to_string())
        })?;

        let (global_labels, global_props) = fact::to_node(global_fact)?;
        let about_props = fact::about_props(about)?;
        let promoted_props = lineage_props(&promoted.temporal)?;
        let (ledger_labels, ledger_props) = to_node(ledger)?;
        let (audit_labels, audit_props) = audit::to_node(audit)?;
        let audit_edge = db_string(Audit::LABEL)?;

        let mut txn = self.graph().begin_write();
        let ids = {
            let mut mutator = txn.mutator();
            // Probe for idempotency / partial-write recovery against the in-txn working graph,
            // under the write lock — so each probe and its write are atomic and no concurrent
            // committer can slip a duplicate global node, ledger row, or audit between them. The
            // UNIQUE ids are the commit-time backstop; this keeps the heal decision consistent.
            let existing_global =
                crate::attestation::fact_node_in(mutator.read(), &global_fact.identity.id)?;
            let existing_ledger = find_by_candidate(mutator.read(), &ledger.candidate_fact_id)?;
            let existing_audit = audit::find_existing(mutator.read(), &audit.identity.id)?;
            let global_node = match existing_global {
                Some(node) => node,
                None => mutator.create_node(global_labels, global_props)?,
            };
            // ABOUT + PROMOTED_TO: write-when-absent so a partial prior write is healed and a
            // replay is a no-op.
            materialize::ensure_edge(
                &mut mutator,
                About::LABEL,
                global_node,
                subject_entity,
                about_props,
            )?;
            materialize::ensure_edge(
                &mut mutator,
                PromotedTo::LABEL,
                team_fact,
                global_node,
                promoted_props,
            )?;
            match existing_ledger {
                Some(node) => {
                    mutator.update_node(node, LabelDiff::new([], [])?, resolution_diff(ledger)?)?;
                }
                None => {
                    mutator.create_node(ledger_labels, ledger_props)?;
                }
            }
            let audit_node = match existing_audit {
                Some(node) => node,
                None => {
                    let node = mutator.create_node(audit_labels, audit_props)?;
                    mutator.create_edge(
                        audit_edge,
                        node,
                        team_fact,
                        PropertyMap::from_pairs(Vec::new())?,
                    )?;
                    node
                }
            };
            PromoteWriteIds {
                global_fact: global_node,
                audit: audit_node,
            }
        };
        txn.commit()?;
        Ok(ids)
    }

    /// Demote a promoted global fact on lost support, atomically and idempotently (06 §4).
    ///
    /// One commit writes the `global -DEMOTED_FROM-> team` lineage edge, quarantines the
    /// global copy (`expired_at = now` and `status = quarantined`, so the current-support
    /// provider drops it), flips the ledger row to `rejected`, and writes the `demote` and
    /// `quarantine` audits with their `AUDIT` edges to the global copy. **The namespace
    /// original is left untouched** — demotion is reversible and never destructive (06 §4).
    ///
    /// Idempotent: the lineage edge is written only when absent; the quarantine is skipped
    /// when the copy is already expired; the ledger is updated in place; the audits are
    /// content-addressed and deduped. A replay (or a double trigger — the structural one now,
    /// the reliability-decay one in M4.T05) collapses to one demotion.
    ///
    /// # Errors
    /// Returns [`StoreError::Invariant`] if the `DEMOTED_FROM` window is out of order;
    /// [`StoreError`] if a translation, mutation, or the commit fails. Nothing is published
    /// if any step fails.
    #[allow(clippy::too_many_arguments)]
    pub fn demote_fact(
        &self,
        global_fact: NodeId,
        team_fact: NodeId,
        demoted: &DemotedFrom,
        now: &Timestamp,
        ledger: &Promotion,
        demote_audit: &AuditEvent,
        quarantine_audit: &AuditEvent,
    ) -> Result<(), StoreError> {
        if !demoted.temporal.windows_ordered() {
            return Err(StoreError::invariant(
                "DEMOTED_FROM window bounds are out of order".to_string(),
            ));
        }

        let demoted_props = lineage_props(&demoted.temporal)?;
        let (ledger_labels, ledger_props) = to_node(ledger)?;
        let audit_edge = db_string(Audit::LABEL)?;

        let mut txn = self.graph().begin_write();
        {
            let mut mutator = txn.mutator();
            // Probe under the write lock against the in-txn working graph, so the
            // already-quarantined guard and the ledger/audit dedup are atomic with their writes.
            // Key the short-circuit on the status mirror, not bare `expired_at` presence: an
            // expiry written by another path (e.g. a future supersession writer) must still get
            // `status = quarantined` here, keeping `expired_at` and `status` set together (06 §4).
            let already_quarantined = {
                let status_key = db_string(STATUS)?;
                mutator
                    .read()
                    .node_properties(global_fact)
                    .and_then(|props| props.get(&status_key).cloned())
                    .map(|value| enum_from_value::<FactStatus>(&value))
                    .transpose()?
                    == Some(FactStatus::Quarantined)
            };
            let existing_ledger = find_by_candidate(mutator.read(), &ledger.candidate_fact_id)?;
            let existing_demote = audit::find_existing(mutator.read(), &demote_audit.identity.id)?;
            let existing_quarantine =
                audit::find_existing(mutator.read(), &quarantine_audit.identity.id)?;

            materialize::ensure_edge(
                &mut mutator,
                DemotedFrom::LABEL,
                global_fact,
                team_fact,
                demoted_props,
            )?;
            if !already_quarantined {
                mutator.update_node(
                    global_fact,
                    LabelDiff::new([], [])?,
                    PropertyDiff::new(
                        [
                            (key(EXPIRED_AT)?, timestamp_value(now)),
                            (key(STATUS)?, enum_value(&FactStatus::Quarantined)?),
                        ],
                        [],
                    )?,
                )?;
            }
            match existing_ledger {
                Some(node) => {
                    mutator.update_node(node, LabelDiff::new([], [])?, resolution_diff(ledger)?)?;
                }
                None => {
                    mutator.create_node(ledger_labels, ledger_props)?;
                }
            }
            if existing_demote.is_none() {
                let (labels, props) = audit::to_node(demote_audit)?;
                let node = mutator.create_node(labels, props)?;
                mutator.create_edge(
                    audit_edge.clone(),
                    node,
                    global_fact,
                    PropertyMap::from_pairs(Vec::new())?,
                )?;
            }
            if existing_quarantine.is_none() {
                let (labels, props) = audit::to_node(quarantine_audit)?;
                let node = mutator.create_node(labels, props)?;
                mutator.create_edge(
                    audit_edge,
                    node,
                    global_fact,
                    PropertyMap::from_pairs(Vec::new())?,
                )?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// The `NodeId` of a fact's `ABOUT` neighbor (its canonical subject entity), if it has one.
    fn about_neighbor(&self, fact: NodeId) -> Result<Option<NodeId>, StoreError> {
        let snapshot = self.graph().read();
        let about_label = db_string(About::LABEL)?;
        Ok(snapshot.outgoing_edges(fact).and_then(|adjacency| {
            adjacency
                .iter_label(&about_label)
                .next()
                .map(|edge| edge.neighbor)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{from_properties, to_node};
    use aionforge_domain::blocks::Identity;
    use aionforge_domain::ids::Id;
    use aionforge_domain::namespace::Namespace;
    use aionforge_domain::nodes::forensic::{Promotion, PromotionStatus};
    use aionforge_domain::time::Timestamp;

    fn ts() -> Timestamp {
        "2026-06-08T09:00:00-05:00[America/Chicago]"
            .parse()
            .expect("valid zoned datetime")
    }

    /// A `pending` ledger row exercises the `None` branches of `resolved_at`/`promoted_fact_id`,
    /// which no store write path produces (the substrate persists only terminal rows), so this
    /// guards the round-trip of the optional fields directly.
    #[test]
    fn a_pending_ledger_row_round_trips_with_its_optional_fields_unset() {
        let row = Promotion {
            identity: Identity {
                id: Id::generate(),
                ingested_at: ts(),
                namespace: Namespace::System,
                expired_at: None,
            },
            candidate_fact_id: Id::generate(),
            posterior: 0.5,
            k: 2,
            status: PromotionStatus::Pending,
            resolved_at: None,
            promoted_fact_id: None,
        };
        let (_, props) = to_node(&row).expect("to_node");
        let back = from_properties(&props).expect("from_properties");
        assert_eq!(row, back);
    }
}
