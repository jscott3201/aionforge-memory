//! Translation between domain semantic types and selene-db nodes/edges (02 §4.2, §5).
//!
//! The [`Fact`] node and its three bi-temporal edges — `ABOUT` (the fact's validity
//! window over its subject entity), `SUPERSEDED_BY`, and `CONTRADICTS` — are written
//! here. Property keys are the selene-db column names from data-model spec §4.2/§5.
//!
//! The domain [`ObjectValue`] collapses the spec's `object_kind` / `object_entity_id`
//! / `object_value` columns into one typed enum; [`object_columns`] splits it back out
//! for storage and [`object_from_columns`] reassembles it. An entity object stores the
//! referenced id in `object_entity_id` (the indexed column) and leaves `object_value`
//! unset; every other variant stores its tagged JSON in `object_value`.
//!
//! The four bi-temporal timestamps live on the edges, never on the `Fact` node
//! (currentness is edge presence, 02 §4.2; GAP-4): [`bitemporal_pairs`] writes them.

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::edges::{About, Contradicts, SupersededBy};
use aionforge_domain::nodes::semantic::Fact;
use aionforge_domain::time::BiTemporal;
use aionforge_domain::value::ObjectValue;
use selene_core::{DbString, LabelSet, PropertyMap, Value, db_string};

use crate::convert::{
    as_embedder_model, as_embedding, as_f64, as_id, as_namespace, as_str, as_timestamp, as_u64,
    embedder_model_value, embedding_value, enum_from_value, enum_value, id_value, json_from_value,
    json_value, key, namespace_value, string_value, timestamp_value,
};
use crate::error::StoreError;

// Identity block (§3).
const ID: &str = "id";
const INGESTED_AT: &str = "ingested_at";
const NAMESPACE: &str = "namespace";
const EXPIRED_AT: &str = "expired_at";
// Stats block (§3).
const IMPORTANCE: &str = "importance";
const TRUST: &str = "trust";
const LAST_ACCESS: &str = "last_access";
const ACCESS_COUNT_RECENT: &str = "access_count_recent";
const REFERENCED_COUNT: &str = "referenced_count";
const SURPRISE: &str = "surprise";
const IS_PINNED: &str = "is_pinned";
// Fact per-kind fields (§4.2).
const SUBJECT_ID: &str = "subject_id";
const PREDICATE: &str = "predicate";
const OBJECT_KIND: &str = "object_kind";
const OBJECT_ENTITY_ID: &str = "object_entity_id";
const OBJECT_VALUE: &str = "object_value";
const CONFIDENCE: &str = "confidence";
const STATUS: &str = "status";
const STATEMENT: &str = "statement";
const EMBEDDING: &str = "embedding_v1";
const EMBEDDER_MODEL: &str = "embedder_model";
const EXTRACTION: &str = "extraction";
// Bi-temporal edge fields (§5).
const VALID_FROM: &str = "valid_from";
const VALID_TO: &str = "valid_to";
// Edge-specific fields (§5).
const REASON: &str = "reason";
const DETECTED_BY: &str = "detected_by";

/// The selene-db node label for a fact (mirrors [`Fact::LABEL`]).
pub(crate) fn label() -> Result<LabelSet, StoreError> {
    Ok(LabelSet::single(db_string(Fact::LABEL)?))
}

/// Split an [`ObjectValue`] into the `(object_kind, object_entity_id?, object_value?)`
/// column triple. An entity reference goes in the indexed `object_entity_id` column;
/// every other variant serializes as its tagged JSON into `object_value`.
fn object_columns(object: &ObjectValue) -> Result<Vec<(DbString, Value)>, StoreError> {
    let mut pairs = vec![(key(OBJECT_KIND)?, enum_value(&object.kind())?)];
    match object {
        ObjectValue::Entity(id) => pairs.push((key(OBJECT_ENTITY_ID)?, id_value(id)?)),
        other => pairs.push((key(OBJECT_VALUE)?, json_value(other)?)),
    }
    Ok(pairs)
}

/// Reassemble an [`ObjectValue`] from the stored columns. An `entity` kind reads the
/// referenced id; every other kind deserializes the tagged JSON, which reconstructs
/// the original variant directly.
fn object_from_columns(
    kind: &str,
    entity_id: Option<&Value>,
    object_value: Option<&Value>,
) -> Result<ObjectValue, StoreError> {
    if kind == "entity" {
        let id = entity_id.ok_or_else(|| {
            StoreError::decode("entity object is missing `object_entity_id`".to_string())
        })?;
        Ok(ObjectValue::Entity(as_id(id)?))
    } else {
        let value = object_value.ok_or_else(|| {
            StoreError::decode(format!("`{kind}` object is missing `object_value`"))
        })?;
        json_from_value(value)
    }
}

/// Translate a [`Fact`] into the `(labels, properties)` pair for `create_node`.
///
/// Nullable fields that are `None` are omitted (an absent property reads back as
/// `None`). The bi-temporal window is not on the node — it lives on the `ABOUT` edge.
pub(crate) fn to_node(fact: &Fact) -> Result<(LabelSet, PropertyMap), StoreError> {
    // 4 identity + 7 stats + up to 11 per-kind fields.
    let mut pairs: Vec<(DbString, Value)> = Vec::with_capacity(22);

    // Identity block.
    pairs.push((key(ID)?, id_value(&fact.identity.id)?));
    pairs.push((
        key(INGESTED_AT)?,
        timestamp_value(&fact.identity.ingested_at),
    ));
    pairs.push((key(NAMESPACE)?, namespace_value(&fact.identity.namespace)?));
    if let Some(expired_at) = &fact.identity.expired_at {
        pairs.push((key(EXPIRED_AT)?, timestamp_value(expired_at)));
    }

    // Stats block.
    pairs.push((key(IMPORTANCE)?, Value::Float(fact.stats.importance)));
    pairs.push((key(TRUST)?, Value::Float(fact.stats.trust)));
    pairs.push((key(LAST_ACCESS)?, timestamp_value(&fact.stats.last_access)));
    pairs.push((
        key(ACCESS_COUNT_RECENT)?,
        Value::Uint(fact.stats.access_count_recent),
    ));
    pairs.push((
        key(REFERENCED_COUNT)?,
        Value::Uint(fact.stats.referenced_count),
    ));
    pairs.push((key(SURPRISE)?, Value::Float(fact.stats.surprise)));
    pairs.push((key(IS_PINNED)?, Value::Bool(fact.stats.is_pinned)));

    // Per-kind fields.
    pairs.push((key(SUBJECT_ID)?, id_value(&fact.subject_id)?));
    pairs.push((key(PREDICATE)?, string_value(&fact.predicate)?));
    pairs.extend(object_columns(&fact.object)?);
    pairs.push((key(CONFIDENCE)?, Value::Float(fact.confidence)));
    pairs.push((key(STATUS)?, enum_value(&fact.status)?));
    pairs.push((key(STATEMENT)?, string_value(&fact.statement)?));
    if let Some(embedding) = &fact.embedding {
        pairs.push((key(EMBEDDING)?, embedding_value(embedding)?));
    }
    if let Some(model) = &fact.embedder_model {
        pairs.push((key(EMBEDDER_MODEL)?, embedder_model_value(model)?));
    }
    if let Some(extraction) = &fact.extraction {
        pairs.push((key(EXTRACTION)?, json_value(extraction)?));
    }

    Ok((label()?, PropertyMap::from_pairs(pairs)?))
}

/// Reconstruct a [`Fact`] from a node's stored property map.
pub(crate) fn from_properties(props: &PropertyMap) -> Result<Fact, StoreError> {
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
    let stats = Stats {
        importance: as_f64(require(IMPORTANCE)?)?,
        trust: as_f64(require(TRUST)?)?,
        last_access: as_timestamp(require(LAST_ACCESS)?)?,
        access_count_recent: as_u64(require(ACCESS_COUNT_RECENT)?)?,
        referenced_count: as_u64(require(REFERENCED_COUNT)?)?,
        surprise: as_f64(require(SURPRISE)?)?,
        is_pinned: crate::convert::as_bool(require(IS_PINNED)?)?,
    };
    let object = object_from_columns(
        as_str(require(OBJECT_KIND)?)?,
        get(OBJECT_ENTITY_ID)?,
        get(OBJECT_VALUE)?,
    )?;

    Ok(Fact {
        identity,
        stats,
        subject_id: as_id(require(SUBJECT_ID)?)?,
        predicate: as_str(require(PREDICATE)?)?.to_string(),
        object,
        confidence: as_f64(require(CONFIDENCE)?)?,
        status: enum_from_value(require(STATUS)?)?,
        statement: as_str(require(STATEMENT)?)?.to_string(),
        embedding: get(EMBEDDING)?.map(as_embedding).transpose()?,
        embedder_model: get(EMBEDDER_MODEL)?.map(as_embedder_model).transpose()?,
        extraction: get(EXTRACTION)?.map(json_from_value).transpose()?,
    })
}

/// The four bi-temporal timestamp pairs shared by every bi-temporal edge (§5).
/// `valid_to`/`expired_at` are omitted when open (`None`).
fn bitemporal_pairs(temporal: &BiTemporal) -> Vec<(DbString, Value)> {
    let mut pairs = Vec::with_capacity(4);
    // key() only fails on an interior NUL, impossible for these static identifiers.
    pairs.push((
        db_string(VALID_FROM).unwrap(),
        timestamp_value(&temporal.valid_from),
    ));
    if let Some(valid_to) = &temporal.valid_to {
        pairs.push((db_string(VALID_TO).unwrap(), timestamp_value(valid_to)));
    }
    pairs.push((
        db_string(INGESTED_AT).unwrap(),
        timestamp_value(&temporal.ingested_at),
    ));
    if let Some(expired_at) = &temporal.expired_at {
        pairs.push((db_string(EXPIRED_AT).unwrap(), timestamp_value(expired_at)));
    }
    pairs
}

/// Read a [`BiTemporal`] block back from an edge's stored property map.
fn bitemporal_from_properties(props: &PropertyMap) -> Result<BiTemporal, StoreError> {
    let get =
        |name: &str| -> Result<Option<&Value>, StoreError> { Ok(props.get(&db_string(name)?)) };
    Ok(BiTemporal {
        valid_from: as_timestamp(get(VALID_FROM)?.ok_or_else(|| {
            StoreError::decode("edge missing required property `valid_from`".to_string())
        })?)?,
        valid_to: get(VALID_TO)?.map(as_timestamp).transpose()?,
        ingested_at: as_timestamp(get(INGESTED_AT)?.ok_or_else(|| {
            StoreError::decode("edge missing required property `ingested_at`".to_string())
        })?)?,
        expired_at: get(EXPIRED_AT)?.map(as_timestamp).transpose()?,
    })
}

/// The `ABOUT` edge property map (the fact's validity window).
pub(crate) fn about_props(about: &About) -> Result<PropertyMap, StoreError> {
    Ok(PropertyMap::from_pairs(bitemporal_pairs(&about.temporal))?)
}

/// The `SUPERSEDED_BY` edge property map (reason + validity window).
pub(crate) fn superseded_by_props(edge: &SupersededBy) -> Result<PropertyMap, StoreError> {
    let mut pairs = vec![(key(REASON)?, string_value(&edge.reason)?)];
    pairs.extend(bitemporal_pairs(&edge.temporal));
    Ok(PropertyMap::from_pairs(pairs)?)
}

/// The `CONTRADICTS` edge property map (detector + validity window).
pub(crate) fn contradicts_props(edge: &Contradicts) -> Result<PropertyMap, StoreError> {
    let mut pairs = vec![(key(DETECTED_BY)?, string_value(&edge.detected_by)?)];
    pairs.extend(bitemporal_pairs(&edge.temporal));
    Ok(PropertyMap::from_pairs(pairs)?)
}

/// Read an `ABOUT` edge back into its domain form.
pub(crate) fn about_from_properties(props: &PropertyMap) -> Result<About, StoreError> {
    Ok(About {
        temporal: bitemporal_from_properties(props)?,
    })
}
