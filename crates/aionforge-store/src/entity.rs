//! Translation between a domain [`Entity`] and a selene-db node (02 §4.3).
//!
//! The canonical referent many surface forms resolve to. Entities are the target of
//! the `ABOUT` edge, so the fact write path needs this primitive; full entity
//! resolution (canonicalization) is a consolidation product (M2.T04). The domain
//! `entity_type` field maps to the reserved-word-safe `type` column.

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::nodes::semantic::Entity;
use selene_core::{DbString, LabelSet, PropertyMap, Value, db_string};

use crate::convert::{
    as_bool, as_embedder_model, as_embedding, as_f64, as_id, as_namespace, as_str, as_timestamp,
    as_u64, embedder_model_value, embedding_value, id_value, json_from_value, json_value, key,
    namespace_value, string_value, timestamp_value,
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
// Entity per-kind fields (§4.3).
const CANONICAL_NAME: &str = "canonical_name";
const TYPE: &str = "type";
const ALIASES: &str = "aliases";
const DESCRIPTION: &str = "description";
const EMBEDDING: &str = "embedding_v1";
const EMBEDDER_MODEL: &str = "embedder_model";
const ATTRIBUTES: &str = "attributes";

/// The selene-db node label for an entity (mirrors [`Entity::LABEL`]).
pub(crate) fn label() -> Result<LabelSet, StoreError> {
    Ok(LabelSet::single(db_string(Entity::LABEL)?))
}

/// Translate an [`Entity`] into the `(labels, properties)` pair for `create_node`.
pub(crate) fn to_node(entity: &Entity) -> Result<(LabelSet, PropertyMap), StoreError> {
    let mut pairs: Vec<(DbString, Value)> = Vec::with_capacity(18);

    // Identity block.
    pairs.push((key(ID)?, id_value(&entity.identity.id)?));
    pairs.push((
        key(INGESTED_AT)?,
        timestamp_value(&entity.identity.ingested_at),
    ));
    pairs.push((
        key(NAMESPACE)?,
        namespace_value(&entity.identity.namespace)?,
    ));
    if let Some(expired_at) = &entity.identity.expired_at {
        pairs.push((key(EXPIRED_AT)?, timestamp_value(expired_at)));
    }

    // Stats block.
    pairs.push((key(IMPORTANCE)?, Value::Float(entity.stats.importance)));
    pairs.push((key(TRUST)?, Value::Float(entity.stats.trust)));
    pairs.push((
        key(LAST_ACCESS)?,
        timestamp_value(&entity.stats.last_access),
    ));
    pairs.push((
        key(ACCESS_COUNT_RECENT)?,
        Value::Uint(entity.stats.access_count_recent),
    ));
    pairs.push((
        key(REFERENCED_COUNT)?,
        Value::Uint(entity.stats.referenced_count),
    ));
    pairs.push((key(SURPRISE)?, Value::Float(entity.stats.surprise)));
    pairs.push((key(IS_PINNED)?, Value::Bool(entity.stats.is_pinned)));

    // Per-kind fields.
    pairs.push((key(CANONICAL_NAME)?, string_value(&entity.canonical_name)?));
    pairs.push((key(TYPE)?, string_value(&entity.entity_type)?));
    if !entity.aliases.is_empty() {
        let items = entity
            .aliases
            .iter()
            .map(|a| string_value(a))
            .collect::<Result<Vec<_>, _>>()?;
        pairs.push((key(ALIASES)?, Value::List(items)));
    }
    if let Some(description) = &entity.description {
        pairs.push((key(DESCRIPTION)?, string_value(description)?));
    }
    if let Some(embedding) = &entity.embedding {
        pairs.push((key(EMBEDDING)?, embedding_value(embedding)?));
    }
    if let Some(model) = &entity.embedder_model {
        pairs.push((key(EMBEDDER_MODEL)?, embedder_model_value(model)?));
    }
    if let Some(attributes) = &entity.attributes {
        pairs.push((key(ATTRIBUTES)?, json_value(attributes)?));
    }

    Ok((label()?, PropertyMap::from_pairs(pairs)?))
}

/// Reconstruct an [`Entity`] from a node's stored property map.
pub(crate) fn from_properties(props: &PropertyMap) -> Result<Entity, StoreError> {
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
        is_pinned: as_bool(require(IS_PINNED)?)?,
    };
    let aliases = match get(ALIASES)? {
        Some(Value::List(items)) => items
            .iter()
            .map(|v| Ok(as_str(v)?.to_string()))
            .collect::<Result<Vec<_>, StoreError>>()?,
        _ => Vec::new(),
    };

    Ok(Entity {
        identity,
        stats,
        canonical_name: as_str(require(CANONICAL_NAME)?)?.to_string(),
        entity_type: as_str(require(TYPE)?)?.to_string(),
        aliases,
        description: get(DESCRIPTION)?
            .map(as_str)
            .transpose()?
            .map(str::to_string),
        embedding: get(EMBEDDING)?.map(as_embedding).transpose()?,
        embedder_model: get(EMBEDDER_MODEL)?.map(as_embedder_model).transpose()?,
        attributes: get(ATTRIBUTES)?.map(json_from_value).transpose()?,
    })
}
