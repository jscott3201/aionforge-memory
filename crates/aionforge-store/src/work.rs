//! Translation between a domain [`WorkItem`] and a selene-db node, plus the work-item
//! write/read surface (work-structure design §2).
//!
//! A work item is the general primitive for *what an agent is doing*: a unit of work at a
//! caller-defined `level`, wired into a hierarchy by the indexed self-referential
//! `parent_id` scalar and advanced through the `work_status` lifecycle. It is Identity-only
//! (no Stats block) and is exempt from decay/forgetting by absence from the maintenance scan
//! sets — so this surface deliberately stays out of the forget/pin/erase machinery; PR1
//! lays down the create + the indexed readers (by id, by parent, by status). The status
//! transition (a guarded CAS plus a signed audit record) and the tag/parent edge writes are
//! later work; here `work_status` is written once at create from the item's value.

use aionforge_domain::blocks::Identity;
use aionforge_domain::nodes::work::{WorkItem, WorkStatus};
use selene_core::{DbString, LabelSet, NodeId, PropertyMap, Value, db_string};
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
}
