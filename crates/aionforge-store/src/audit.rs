//! Translation between a domain [`AuditEvent`] and a selene-db node (02 §4.11).
//!
//! A forensic kind carrying only the [`Identity`] block (no `Stats`). The `payload`
//! is an intentionally open `JSON` shape (02 §6.4), round-tripped as a native JSON
//! value; `kind` serializes to its `snake_case` spec string.

use aionforge_domain::blocks::Identity;
use aionforge_domain::ids::Id;
use aionforge_domain::nodes::forensic::AuditEvent;
use selene_core::{DbString, LabelSet, NodeId, PropertyMap, Value, db_string};
use selene_graph::{RowIndex, SeleneGraph};

use crate::convert::{
    as_id, as_namespace, as_str, as_timestamp, enum_from_value, enum_value, id_value,
    json_from_value, json_value, key, namespace_value, string_value, timestamp_value,
};
use crate::error::StoreError;

const ID: &str = "id";
const INGESTED_AT: &str = "ingested_at";
const NAMESPACE: &str = "namespace";
const EXPIRED_AT: &str = "expired_at";
const KIND: &str = "kind";
const SUBJECT_ID: &str = "subject_id";
const ACTOR_ID: &str = "actor_id";
const PAYLOAD: &str = "payload";
const SIGNATURE: &str = "signature";
const OCCURRED_AT: &str = "occurred_at";

/// The selene-db node label for an audit event.
pub(crate) fn label() -> Result<LabelSet, StoreError> {
    Ok(LabelSet::single(db_string(AuditEvent::LABEL)?))
}

/// Translate an [`AuditEvent`] into `(labels, properties)` for `create_node`.
pub(crate) fn to_node(event: &AuditEvent) -> Result<(LabelSet, PropertyMap), StoreError> {
    let mut pairs: Vec<(DbString, Value)> = Vec::with_capacity(10);

    pairs.push((key(ID)?, id_value(&event.identity.id)?));
    pairs.push((
        key(INGESTED_AT)?,
        timestamp_value(&event.identity.ingested_at),
    ));
    pairs.push((key(NAMESPACE)?, namespace_value(&event.identity.namespace)?));
    if let Some(expired_at) = &event.identity.expired_at {
        pairs.push((key(EXPIRED_AT)?, timestamp_value(expired_at)));
    }
    pairs.push((key(KIND)?, enum_value(&event.kind)?));
    pairs.push((key(SUBJECT_ID)?, id_value(&event.subject_id)?));
    pairs.push((key(ACTOR_ID)?, id_value(&event.actor_id)?));
    pairs.push((key(PAYLOAD)?, json_value(&event.payload)?));
    pairs.push((key(SIGNATURE)?, string_value(&event.signature)?));
    pairs.push((key(OCCURRED_AT)?, timestamp_value(&event.occurred_at)));

    Ok((label()?, PropertyMap::from_pairs(pairs)?))
}

/// Reconstruct an [`AuditEvent`] from a node's stored property map.
pub(crate) fn from_properties(props: &PropertyMap) -> Result<AuditEvent, StoreError> {
    let get =
        |name: &str| -> Result<Option<&Value>, StoreError> { Ok(props.get(&db_string(name)?)) };
    let require = |name: &str| -> Result<&Value, StoreError> {
        get(name)?.ok_or_else(|| StoreError::decode(format!("missing required property `{name}`")))
    };

    Ok(AuditEvent {
        identity: Identity {
            id: as_id(require(ID)?)?,
            ingested_at: as_timestamp(require(INGESTED_AT)?)?,
            namespace: as_namespace(require(NAMESPACE)?)?,
            expired_at: get(EXPIRED_AT)?.map(as_timestamp).transpose()?,
        },
        kind: enum_from_value(require(KIND)?)?,
        subject_id: as_id(require(SUBJECT_ID)?)?,
        actor_id: as_id(require(ACTOR_ID)?)?,
        payload: json_from_value(require(PAYLOAD)?)?,
        signature: as_str(require(SIGNATURE)?)?.to_string(),
        occurred_at: as_timestamp(require(OCCURRED_AT)?)?,
    })
}

/// Find an audit event already written with this content-addressed id, returning its node.
/// `AuditEvent.id` is `UNIQUE`, so this is a probe — the dedup that makes a replay of the same
/// episode write no second copy of an audit it already produced (04 §3), mirroring the fact
/// and note paths.
pub(crate) fn find_existing(snapshot: &SeleneGraph, id: &Id) -> Result<Option<NodeId>, StoreError> {
    let label = db_string(AuditEvent::LABEL)?;
    let prop = db_string(ID)?;
    let value = id_value(id)?;
    let Some(rows) = snapshot.nodes_with_property_eq(&label, &prop, &value) else {
        return Ok(None);
    };
    Ok(rows
        .iter()
        .find_map(|row| snapshot.node_id_for_row(RowIndex::new(row))))
}
