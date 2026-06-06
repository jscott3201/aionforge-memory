//! Translation between a domain [`Episode`] and a selene-db node.
//!
//! Property keys are the selene-db column names from data-model spec §4.1 (plus the
//! shared identity/stats blocks, §3). Domain field names that differ from the spec
//! column name are mapped here — the domain holds a single logical `embedding`,
//! stored under the versioned `embedding_v1` property (§7). Other memory kinds get
//! their own translation modules as the milestones that write them land; `Episode`
//! is the M0 exit kind.

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::nodes::episodic::Episode;
use selene_core::{DbString, LabelSet, PropertyMap, Value, db_string};

use crate::convert::{
    as_bool, as_content_hash, as_embedder_model, as_embedding, as_f64, as_id, as_namespace, as_str,
    as_timestamp, as_u64, embedder_model_value, embedding_value, enum_from_value, enum_value,
    hash_value, id_value, json_from_value, json_value, key, namespace_value, string_value,
    timestamp_value,
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
// Episode per-kind fields (§4.1).
const CONTENT: &str = "content";
const ROLE: &str = "role";
const CAPTURED_AT: &str = "captured_at";
const AGENT_ID: &str = "agent_id";
const SESSION_ID: &str = "session_id";
const CONTENT_HASH: &str = "content_hash";
const EMBEDDING: &str = "embedding_v1";
const EMBEDDER_MODEL: &str = "embedder_model";
const CONSOLIDATION_STATE: &str = "consolidation_state";
const ORIGIN: &str = "origin";

/// The selene-db node label for an episode (mirrors [`Episode::LABEL`]).
pub(crate) fn label() -> Result<LabelSet, StoreError> {
    Ok(LabelSet::single(db_string(Episode::LABEL)?))
}

/// Translate an [`Episode`] into the `(labels, properties)` pair for `create_node`.
///
/// Nullable fields that are `None` are omitted from the property map (an absent
/// property reads back as `None`).
pub(crate) fn to_node(episode: &Episode) -> Result<(LabelSet, PropertyMap), StoreError> {
    // 4 identity + 7 stats + 10 per-kind fields when every nullable field is present.
    let mut pairs: Vec<(DbString, Value)> = Vec::with_capacity(21);

    // Identity block.
    pairs.push((key(ID)?, id_value(&episode.identity.id)?));
    pairs.push((
        key(INGESTED_AT)?,
        timestamp_value(&episode.identity.ingested_at),
    ));
    pairs.push((
        key(NAMESPACE)?,
        namespace_value(&episode.identity.namespace)?,
    ));
    if let Some(expired_at) = &episode.identity.expired_at {
        pairs.push((key(EXPIRED_AT)?, timestamp_value(expired_at)));
    }

    // Stats block.
    pairs.push((key(IMPORTANCE)?, Value::Float(episode.stats.importance)));
    pairs.push((key(TRUST)?, Value::Float(episode.stats.trust)));
    pairs.push((
        key(LAST_ACCESS)?,
        timestamp_value(&episode.stats.last_access),
    ));
    pairs.push((
        key(ACCESS_COUNT_RECENT)?,
        Value::Uint(episode.stats.access_count_recent),
    ));
    pairs.push((
        key(REFERENCED_COUNT)?,
        Value::Uint(episode.stats.referenced_count),
    ));
    pairs.push((key(SURPRISE)?, Value::Float(episode.stats.surprise)));
    pairs.push((key(IS_PINNED)?, Value::Bool(episode.stats.is_pinned)));

    // Per-kind fields.
    pairs.push((key(CONTENT)?, string_value(&episode.content)?));
    pairs.push((key(ROLE)?, enum_value(&episode.role)?));
    pairs.push((key(CAPTURED_AT)?, timestamp_value(&episode.captured_at)));
    pairs.push((key(AGENT_ID)?, id_value(&episode.agent_id)?));
    if let Some(session_id) = &episode.session_id {
        pairs.push((key(SESSION_ID)?, id_value(session_id)?));
    }
    pairs.push((key(CONTENT_HASH)?, hash_value(&episode.content_hash)?));
    if let Some(embedding) = &episode.embedding {
        pairs.push((key(EMBEDDING)?, embedding_value(embedding)?));
    }
    if let Some(model) = &episode.embedder_model {
        pairs.push((key(EMBEDDER_MODEL)?, embedder_model_value(model)?));
    }
    pairs.push((
        key(CONSOLIDATION_STATE)?,
        enum_value(&episode.consolidation_state)?,
    ));
    if let Some(origin) = &episode.origin {
        pairs.push((key(ORIGIN)?, json_value(origin)?));
    }

    Ok((label()?, PropertyMap::from_pairs(pairs)?))
}

/// Reconstruct an [`Episode`] from a node's stored property map.
pub(crate) fn from_properties(props: &PropertyMap) -> Result<Episode, StoreError> {
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

    Ok(Episode {
        identity,
        stats,
        content: as_str(require(CONTENT)?)?.to_string(),
        role: enum_from_value(require(ROLE)?)?,
        captured_at: as_timestamp(require(CAPTURED_AT)?)?,
        agent_id: as_id(require(AGENT_ID)?)?,
        session_id: get(SESSION_ID)?.map(as_id).transpose()?,
        content_hash: as_content_hash(require(CONTENT_HASH)?)?,
        embedding: get(EMBEDDING)?.map(as_embedding).transpose()?,
        embedder_model: get(EMBEDDER_MODEL)?.map(as_embedder_model).transpose()?,
        consolidation_state: enum_from_value(require(CONSOLIDATION_STATE)?)?,
        origin: get(ORIGIN)?.map(json_from_value).transpose()?,
    })
}
