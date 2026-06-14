//! Translation between a domain [`WorkItem`] and a selene-db node, plus the work-item
//! write/read surface (work-structure design §2).
//!
//! A work item is the general primitive for *what an agent is doing*: a unit of work at a
//! caller-defined `level`, wired into a hierarchy by the indexed self-referential
//! `parent_id` scalar and advanced through the `work_status` lifecycle. It is Identity-only
//! (no Stats block) and is exempt from decay/forgetting by absence from the maintenance scan
//! sets — so this surface deliberately stays out of the forget/pin/erase machinery. The create
//! plus the indexed readers (by id, by parent, by status) and [`Store::advance_work_status`] —
//! the guarded compare-and-set status transition that co-commits a signed `WorkStatusChange`
//! audit record — live here. Re-parent/reorder and the tag-edge writes ride the tool surface.

use aionforge_domain::blocks::Identity;
use aionforge_domain::edges::Audit;
use aionforge_domain::ids::Id;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::work::{WorkItem, WorkStatus};
use aionforge_domain::time::Timestamp;
use selene_core::{
    DbString, LabelDiff, LabelSet, NodeId, PropertyDiff, PropertyMap, Value, db_string,
};
use selene_graph::{RowIndex, SeleneGraph};

use crate::convert::{
    as_id, as_namespace, as_str, as_timestamp, as_u64, enum_from_value, enum_value, id_value, key,
    namespace_value, string_value, timestamp_value,
};
use crate::error::StoreError;
use crate::store::Store;

// Identity block (§3).
const ID: &str = "id";
const INGESTED_AT: &str = "ingested_at";
const NAMESPACE: &str = "namespace";
const EXPIRED_AT: &str = "expired_at";
// WorkItem per-kind fields (work-structure design §2).
const TITLE: &str = "title";
const BODY: &str = "body";
const LEVEL: &str = "level";
const WORK_STATUS: &str = "work_status";
const PARENT_ID: &str = "parent_id";
const ORDINAL: &str = "ordinal";

/// The selene-db node label for a work item (mirrors [`WorkItem::LABEL`]).
pub(crate) fn label() -> Result<LabelSet, StoreError> {
    Ok(LabelSet::single(db_string(WorkItem::LABEL)?))
}

/// Translate a [`WorkItem`] into the `(labels, properties)` pair for `create_node`.
pub(crate) fn to_node(item: &WorkItem) -> Result<(LabelSet, PropertyMap), StoreError> {
    let mut pairs: Vec<(DbString, Value)> = Vec::with_capacity(10);

    // Identity block.
    pairs.push((key(ID)?, id_value(&item.identity.id)?));
    pairs.push((
        key(INGESTED_AT)?,
        timestamp_value(&item.identity.ingested_at),
    ));
    pairs.push((key(NAMESPACE)?, namespace_value(&item.identity.namespace)?));
    if let Some(expired_at) = &item.identity.expired_at {
        pairs.push((key(EXPIRED_AT)?, timestamp_value(expired_at)));
    }

    // Per-kind fields.
    pairs.push((key(TITLE)?, string_value(&item.title)?));
    if let Some(body) = &item.body {
        pairs.push((key(BODY)?, string_value(body)?));
    }
    pairs.push((key(LEVEL)?, string_value(&item.level)?));
    pairs.push((key(WORK_STATUS)?, enum_value(&item.work_status)?));
    if let Some(parent_id) = &item.parent_id {
        pairs.push((key(PARENT_ID)?, id_value(parent_id)?));
    }
    pairs.push((key(ORDINAL)?, Value::Uint(item.ordinal)));

    Ok((label()?, PropertyMap::from_pairs(pairs)?))
}

/// Reconstruct a [`WorkItem`] from a node's stored property map.
pub(crate) fn from_properties(props: &PropertyMap) -> Result<WorkItem, StoreError> {
    let get =
        |name: &str| -> Result<Option<&Value>, StoreError> { Ok(props.get(&db_string(name)?)) };
    let require = |name: &str| -> Result<&Value, StoreError> {
        get(name)?.ok_or_else(|| StoreError::decode(format!("missing required property `{name}`")))
    };

    let identity = Identity {
        id: as_id(require(ID)?)?,
        ingested_at: as_timestamp(require(INGESTED_AT)?)?,
        namespace: as_namespace(require(NAMESPACE)?)?,
        expired_at: get(EXPIRED_AT)?.map(as_timestamp).transpose()?,
    };

    Ok(WorkItem {
        identity,
        title: as_str(require(TITLE)?)?.to_string(),
        body: get(BODY)?.map(as_str).transpose()?.map(str::to_string),
        level: as_str(require(LEVEL)?)?.to_string(),
        work_status: enum_from_value(require(WORK_STATUS)?)?,
        parent_id: get(PARENT_ID)?.map(as_id).transpose()?,
        ordinal: as_u64(require(ORDINAL)?)?,
    })
}

/// The committed node carrying this `WorkItem.id` against a read snapshot (`id` is
/// `UNIQUE`-indexed → at most one).
fn work_item_node_id_in(
    snapshot: &SeleneGraph,
    id: &aionforge_domain::ids::Id,
) -> Result<Option<NodeId>, StoreError> {
    let label = db_string(WorkItem::LABEL)?;
    let prop = db_string(ID)?;
    let value = id_value(id)?;
    let Some(rows) = snapshot.nodes_with_property_eq(&label, &prop, &value) else {
        return Ok(None);
    };
    Ok(rows
        .iter()
        .find_map(|row| snapshot.node_id_for_row(RowIndex::new(row))))
}

/// Decode every work-item node whose scalar `property` equals `value` (an indexed probe).
fn work_items_where(
    snapshot: &SeleneGraph,
    property: &str,
    value: &Value,
) -> Result<Vec<WorkItem>, StoreError> {
    let label = db_string(WorkItem::LABEL)?;
    let prop = db_string(property)?;
    let Some(rows) = snapshot.nodes_with_property_eq(&label, &prop, value) else {
        return Ok(Vec::new());
    };
    let mut items = Vec::new();
    for row in rows.iter() {
        let Some(node) = snapshot.node_id_for_row(RowIndex::new(row)) else {
            continue;
        };
        if let Some(props) = snapshot.node_properties(node) {
            items.push(from_properties(props)?);
        }
    }
    Ok(items)
}

impl Store {
    /// Save a work item, returning its node id (work-structure design §2).
    ///
    /// One atomic commit that creates the `WorkItem` node from the item's fields, including
    /// its `work_status` and (when set) its `parent_id`. The caller owns identity and tree
    /// shape; this surface is the mechanical persistence. Parent-resolves and orphan guards
    /// are a higher-layer concern (the tool surface), not enforced here.
    ///
    /// # Errors
    /// Returns [`StoreError`] if translation, the create, or the commit fails.
    pub fn save_work_item(&self, item: &WorkItem) -> Result<NodeId, StoreError> {
        let (labels, props) = to_node(item)?;
        let mut txn = self.graph().begin_write();
        let node = {
            let mut mutator = txn.mutator();
            mutator.create_node(labels, props)?
        };
        txn.commit()?;
        Ok(node)
    }

    /// Read a work item back by its node id from a fresh snapshot.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the stored data cannot be decoded into a [`WorkItem`].
    pub fn work_item_by_node_id(&self, id: NodeId) -> Result<Option<WorkItem>, StoreError> {
        let snapshot = self.graph().read();
        match snapshot.node_properties(id) {
            Some(props) => Ok(Some(from_properties(props)?)),
            None => Ok(None),
        }
    }

    /// Read a work item back by its domain id from a fresh snapshot (`id` is `UNIQUE`, so a
    /// probe).
    ///
    /// # Errors
    /// Returns [`StoreError`] if the lookup fails or the stored data cannot be decoded.
    pub fn work_item_by_id(
        &self,
        id: &aionforge_domain::ids::Id,
    ) -> Result<Option<WorkItem>, StoreError> {
        let snapshot = self.graph().read();
        match work_item_node_id_in(&snapshot, id)? {
            Some(node) => Ok(snapshot
                .node_properties(node)
                .map(from_properties)
                .transpose()?),
            None => Ok(None),
        }
    }

    /// Every direct child of `parent`, ordered by `ordinal` then id (`parent_id` is indexed,
    /// so a probe).
    ///
    /// The containment fan-out: a single indexed `parent_id` probe over the parent's bounded
    /// child set, ordered in Rust by `(ordinal, id)` so siblings come back in declared order
    /// deterministically. Root items (no parent) are not reachable here — absence of a
    /// `parent_id` property is not an equality the index can probe.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the lookup fails or a stored child cannot be decoded.
    pub fn work_items_by_parent(
        &self,
        parent: &aionforge_domain::ids::Id,
    ) -> Result<Vec<WorkItem>, StoreError> {
        let snapshot = self.graph().read();
        let mut items = work_items_where(&snapshot, PARENT_ID, &id_value(parent)?)?;
        items.sort_by(|a, b| {
            a.ordinal
                .cmp(&b.ordinal)
                .then_with(|| a.identity.id.cmp(&b.identity.id))
        });
        Ok(items)
    }

    /// Every work item in this lifecycle `status`, in ascending id order (`work_status` is
    /// indexed, so a probe).
    ///
    /// # Errors
    /// Returns [`StoreError`] if the lookup fails or a stored item cannot be decoded.
    pub fn work_items_by_status(&self, status: WorkStatus) -> Result<Vec<WorkItem>, StoreError> {
        let snapshot = self.graph().read();
        let mut items = work_items_where(&snapshot, WORK_STATUS, &enum_value(&status)?)?;
        items.sort_by_key(|item| item.identity.id);
        Ok(items)
    }

    /// Every work item at this caller-defined `level`, in ascending id order (`level` is indexed,
    /// so a probe). The OPEN level vocabulary means any harness term resolves without a recompile.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the lookup fails or a stored item cannot be decoded.
    pub fn work_items_by_level(&self, level: &str) -> Result<Vec<WorkItem>, StoreError> {
        let snapshot = self.graph().read();
        let mut items = work_items_where(&snapshot, LEVEL, &string_value(level)?)?;
        items.sort_by_key(|item| item.identity.id);
        Ok(items)
    }

    /// Advance a work item's lifecycle status as a guarded compare-and-set, recording the
    /// transition in the signed audit trail (work-structure design §2).
    ///
    /// One atomic commit: resolve the work item by domain id, read its current `work_status`,
    /// and — when `expected_from` is given — refuse with [`StoreError::Invariant`] unless it
    /// matches (the CAS guard that turns a stale-state advance into a clean error rather than a
    /// lost update). The new `work_status` is written in place — the work item stays one node for
    /// life, only this field moves — and a signed `WorkStatusChange` [`AuditEvent`] anchored on
    /// the work item via an `AUDIT` edge is co-committed in the SAME transaction, so the
    /// transition and its audit are all-or-nothing. Lifecycle history therefore lives in the
    /// by-subject audit trail (see [`Store::audit_history`]), never in version nodes. Returns the
    /// updated work item.
    ///
    /// # Errors
    /// Returns [`StoreError::Invariant`] on a CAS mismatch, [`StoreError`] if no work item carries
    /// `id`, or if a mutation or the commit fails.
    pub fn advance_work_status(
        &self,
        id: &Id,
        to: WorkStatus,
        expected_from: Option<WorkStatus>,
        actor: &Id,
        at: &Timestamp,
    ) -> Result<WorkItem, StoreError> {
        let mut txn = self.graph().begin_write();
        let updated = {
            let mut mutator = txn.mutator();
            let node = work_item_node_id_in(mutator.read(), id)?
                .ok_or_else(|| StoreError::decode(format!("work item {id} not found")))?;
            // Read the current item, dropping the read borrow before mutating.
            let current = {
                let read = mutator.read();
                let props = read
                    .node_properties(node)
                    .ok_or_else(|| StoreError::decode("work item vanished mid-transition"))?;
                from_properties(props)?
            };
            let from = current.work_status;
            if let Some(expected) = expected_from
                && from != expected
            {
                return Err(StoreError::invariant(format!(
                    "work item {id} is {from:?}, expected {expected:?} to advance to {to:?}"
                )));
            }
            // State-gate the write: advancing to the status the item already holds is a no-op
            // that builds no audit at all. Idempotency lives in this gate, never in the audit id
            // (the `audit_addr.rs` discipline), so a crash-retry or same-state re-assertion never
            // leaves a phantom `{from: X, to: X}` row in the by-subject history.
            if to == from {
                return Ok(current);
            }
            mutator.update_node(
                node,
                LabelDiff::new([], [])?,
                PropertyDiff::new([(db_string(WORK_STATUS)?, enum_value(&to)?)], [])?,
            )?;
            // Co-commit the signed transition record, anchored on the work item, in this txn.
            // One fresh id per applied transition — deliberately generated, NOT content-addressed
            // (the `audit_addr.rs` discipline): every real flip is its own audit row, so two
            // genuine crossings of the same transition at the same instant never collide into a
            // single id whose second write is silently deduplicated away. With a unique id the
            // `ensure_event` probe never matches, so `created` is always true here; the guard
            // mirrors the canonical sibling sites and keeps the edge in lockstep with the node.
            let audit = AuditEvent {
                identity: Identity {
                    id: Id::generate(),
                    ingested_at: at.clone(),
                    namespace: current.identity.namespace.clone(),
                    expired_at: None,
                },
                kind: AuditKind::WorkStatusChange,
                subject_id: *id,
                actor_id: *actor,
                payload: serde_json::json!({ "from": from, "to": to }),
                signature: String::new(),
                occurred_at: at.clone(),
            };
            let ensured = crate::audit::ensure_event(&mut mutator, &audit, self.audit_signer())?;
            if ensured.created {
                mutator.create_edge(
                    db_string(Audit::LABEL)?,
                    ensured.node,
                    node,
                    PropertyMap::from_pairs(Vec::new())?,
                )?;
            }
            WorkItem {
                work_status: to,
                ..current
            }
        };
        txn.commit()?;
        Ok(updated)
    }

    /// Re-parent a work item, returning the updated item (work-structure design §2).
    ///
    /// One atomic commit: resolve by domain id, read the current `parent_id`, and — when it
    /// already equals `new_parent_id` — return `Ok(current)` with no write (the idempotency gate,
    /// mirroring [`Store::advance_work_status`]). Setting a parent writes the `parent_id` scalar;
    /// clearing it (to a root) REMOVES the property, since absence — not a null — is the "no
    /// parent" semantic the `parent_id` index probes by. Re-parenting is NOT audited (it is not a
    /// lifecycle transition) and NOT cycle-guarded here: the self-parent/cycle/orphan guard is a
    /// higher-layer (tool-surface) concern, kept out of the mechanical persistence.
    ///
    /// # Errors
    /// Returns [`StoreError`] if no work item carries `id`, or a mutation or the commit fails.
    pub fn set_parent(&self, id: &Id, new_parent_id: Option<&Id>) -> Result<WorkItem, StoreError> {
        let mut txn = self.graph().begin_write();
        let updated = {
            let mut mutator = txn.mutator();
            let node = work_item_node_id_in(mutator.read(), id)?
                .ok_or_else(|| StoreError::decode(format!("work item {id} not found")))?;
            let current = {
                let read = mutator.read();
                let props = read
                    .node_properties(node)
                    .ok_or_else(|| StoreError::decode("work item vanished mid-reparent"))?;
                from_properties(props)?
            };
            // No-op gate: re-parenting to the parent it already has writes nothing.
            if current.parent_id.as_ref() == new_parent_id {
                return Ok(current);
            }
            // Set the scalar to a parent, or REMOVE it to make the item a root — absence is the
            // "no parent" the index probes by, so clearing must drop the property, not null it.
            let diff = match new_parent_id {
                Some(parent) => {
                    PropertyDiff::new([(db_string(PARENT_ID)?, id_value(parent)?)], [])?
                }
                None => PropertyDiff::new([], [db_string(PARENT_ID)?])?,
            };
            mutator.update_node(node, LabelDiff::new([], [])?, diff)?;
            WorkItem {
                parent_id: new_parent_id.copied(),
                ..current
            }
        };
        txn.commit()?;
        Ok(updated)
    }

    /// Set a work item's sibling ordinal, returning the updated item (work-structure design §2).
    ///
    /// One atomic commit mirroring [`Store::set_parent`]: a write to the same ordinal it already
    /// holds is a no-op. `ordinal` is a `UINT` scalar, written directly (not through the enum
    /// encoder). Sibling order is read by [`Store::work_items_by_parent`], which sorts by
    /// `(ordinal, id)`. Not audited.
    ///
    /// # Errors
    /// Returns [`StoreError`] if no work item carries `id`, or a mutation or the commit fails.
    pub fn reorder(&self, id: &Id, ordinal: u64) -> Result<WorkItem, StoreError> {
        let mut txn = self.graph().begin_write();
        let updated = {
            let mut mutator = txn.mutator();
            let node = work_item_node_id_in(mutator.read(), id)?
                .ok_or_else(|| StoreError::decode(format!("work item {id} not found")))?;
            let current = {
                let read = mutator.read();
                let props = read
                    .node_properties(node)
                    .ok_or_else(|| StoreError::decode("work item vanished mid-reorder"))?;
                from_properties(props)?
            };
            if current.ordinal == ordinal {
                return Ok(current);
            }
            mutator.update_node(
                node,
                LabelDiff::new([], [])?,
                PropertyDiff::new([(db_string(ORDINAL)?, Value::Uint(ordinal))], [])?,
            )?;
            WorkItem { ordinal, ..current }
        };
        txn.commit()?;
        Ok(updated)
    }
}
