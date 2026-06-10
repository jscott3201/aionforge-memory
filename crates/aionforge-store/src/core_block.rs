//! Translation between a domain [`CoreBlock`] and a selene-db node, plus the
//! identity-tier read surface (02 §4.7, 05 §4, M5.T04).
//!
//! Core blocks are the identity tier: slow, stable persona/commitment/redline
//! statements with an "attested writes only" mutability contract. This slice is the
//! read half — the conversion pair and the two readers the recall bundle and the edit
//! gate resolve through. The write surface (the un-attested genesis create and the
//! attested whole-value edit) builds on these in the next slice, with the
//! second-attester gate enforced *above* the store in the trust-layer orchestrator.
//!
//! One block is **one node under one stable id for its whole life**: an edit swaps the
//! content in place rather than minting a version node, so the id that forgetting
//! resolves to refuse, the erasure cascade purges, the `ATTESTED_BY` edges anchor to,
//! and the M5.T05 drift baseline keys on are all the same id forever. The non-lossy
//! history of an edited block lives in the signed `core_edit` audit trail (prior and
//! new content hashes), not in the graph.

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::ids::Id;
use aionforge_domain::nodes::core::CoreBlock;
use selene_core::{DbString, LabelSet, PropertyMap, Value, db_string};
use selene_graph::RowIndex;

use crate::convert::{
    as_bool, as_embedder_model, as_embedding, as_f64, as_id, as_namespace, as_str, as_timestamp,
    as_u64, embedder_model_value, embedding_value, enum_from_value, enum_value, id_value,
    json_from_value, json_value, key, namespace_value, node_by_id, string_value, timestamp_value,
};
use crate::error::StoreError;
use crate::store::Store;

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
// CoreBlock per-kind fields (§4.7).
const CONTENT: &str = "content";
const BLOCK_KIND: &str = "block_kind";
const SENSITIVITY: &str = "sensitivity";
const DRIFT_BASELINE: &str = "drift_baseline";
const EMBEDDING: &str = "embedding_v1";
const EMBEDDER_MODEL: &str = "embedder_model";

/// The selene-db node label for a core block (mirrors [`CoreBlock::LABEL`]).
pub(crate) fn label() -> Result<LabelSet, StoreError> {
    Ok(LabelSet::single(db_string(CoreBlock::LABEL)?))
}

/// Translate a [`CoreBlock`] into the `(labels, properties)` pair for `create_node`.
pub(crate) fn to_node(block: &CoreBlock) -> Result<(LabelSet, PropertyMap), StoreError> {
    let mut pairs: Vec<(DbString, Value)> = Vec::with_capacity(17);

    // Identity block.
    pairs.push((key(ID)?, id_value(&block.identity.id)?));
    pairs.push((
        key(INGESTED_AT)?,
        timestamp_value(&block.identity.ingested_at),
    ));
    pairs.push((key(NAMESPACE)?, namespace_value(&block.identity.namespace)?));
    if let Some(expired_at) = &block.identity.expired_at {
        pairs.push((key(EXPIRED_AT)?, timestamp_value(expired_at)));
    }

    // Stats block.
    pairs.push((key(IMPORTANCE)?, Value::Float(block.stats.importance)));
    pairs.push((key(TRUST)?, Value::Float(block.stats.trust)));
    pairs.push((key(LAST_ACCESS)?, timestamp_value(&block.stats.last_access)));
    pairs.push((
        key(ACCESS_COUNT_RECENT)?,
        Value::Uint(block.stats.access_count_recent),
    ));
    pairs.push((
        key(REFERENCED_COUNT)?,
        Value::Uint(block.stats.referenced_count),
    ));
    pairs.push((key(SURPRISE)?, Value::Float(block.stats.surprise)));
    pairs.push((key(IS_PINNED)?, Value::Bool(block.stats.is_pinned)));

    // Per-kind fields.
    pairs.push((key(CONTENT)?, string_value(&block.content)?));
    pairs.push((key(BLOCK_KIND)?, enum_value(&block.block_kind)?));
    if let Some(sensitivity) = &block.sensitivity {
        pairs.push((key(SENSITIVITY)?, string_value(sensitivity)?));
    }
    if let Some(baseline) = &block.drift_baseline {
        pairs.push((key(DRIFT_BASELINE)?, json_value(baseline)?));
    }
    if let Some(embedding) = &block.embedding {
        pairs.push((key(EMBEDDING)?, embedding_value(embedding)?));
    }
    if let Some(model) = &block.embedder_model {
        pairs.push((key(EMBEDDER_MODEL)?, embedder_model_value(model)?));
    }

    Ok((label()?, PropertyMap::from_pairs(pairs)?))
}

/// Reconstruct a [`CoreBlock`] from a node's stored property map.
pub(crate) fn from_properties(props: &PropertyMap) -> Result<CoreBlock, StoreError> {
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

    Ok(CoreBlock {
        identity,
        stats,
        content: as_str(require(CONTENT)?)?.to_string(),
        block_kind: enum_from_value(require(BLOCK_KIND)?)?,
        sensitivity: get(SENSITIVITY)?
            .map(as_str)
            .transpose()?
            .map(str::to_string),
        drift_baseline: get(DRIFT_BASELINE)?.map(json_from_value).transpose()?,
        embedding: get(EMBEDDING)?.map(as_embedding).transpose()?,
        embedder_model: get(EMBEDDER_MODEL)?.map(as_embedder_model).transpose()?,
    })
}

impl Store {
    /// Resolve one core block by its stable domain id — live or expired, so the edit
    /// gate and the point ops can name a block that exists rather than claiming it
    /// does not.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a read or decode fails.
    pub fn core_block_by_id(&self, id: &Id) -> Result<Option<CoreBlock>, StoreError> {
        let snapshot = self.graph().read();
        let Some(node) = node_by_id(&snapshot, CoreBlock::LABEL, id)? else {
            return Ok(None);
        };
        let props = snapshot.node_properties(node).ok_or_else(|| {
            StoreError::invariant("resolved core block has no properties".to_string())
        })?;
        Ok(Some(from_properties(props)?))
    }

    /// Every live (unexpired) core block, ordered by id. The recall bundle's
    /// always-include pre-pass reads this and applies the caller's visible-set
    /// namespace check — visibility filtering stays above the store, with the
    /// retriever, like every other recall read (the M4.T01 store push-down stayed
    /// deferred). Identity is small and slow-changing by design, so a label scan is
    /// the right shape.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a read or decode fails.
    pub fn live_core_blocks(&self) -> Result<Vec<CoreBlock>, StoreError> {
        let snapshot = self.graph().read();
        let label = db_string(CoreBlock::LABEL)?;
        let expired_key = db_string(EXPIRED_AT)?;
        let Some(rows) = snapshot.nodes_with_label(&label) else {
            return Ok(Vec::new());
        };
        let mut blocks = Vec::new();
        for row in rows.iter() {
            let Some(node) = snapshot.node_id_for_row(RowIndex::new(row)) else {
                continue;
            };
            let Some(props) = snapshot.node_properties(node) else {
                continue;
            };
            if props.get(&expired_key).is_some() {
                continue;
            }
            blocks.push(from_properties(props)?);
        }
        blocks.sort_by_key(|b| b.identity.id);
        Ok(blocks)
    }
}

#[cfg(test)]
mod tests {
    //! The conversion round-trip needs `to_node` (no public write surface until the
    //! attested-edit slice lands), so these live in-crate; the readers are exercised
    //! through the same raw insert.

    use aionforge_domain::embedding::{EmbedderModel, Embedding};
    use aionforge_domain::namespace::Namespace;
    use aionforge_domain::nodes::core::BlockKind;
    use aionforge_domain::time::Timestamp;

    use super::*;
    use crate::config::StoreConfig;

    fn ts(text: &str) -> Timestamp {
        text.parse().expect("valid zoned datetime literal")
    }

    fn store() -> Store {
        let store = Store::open_with_config(StoreConfig {
            embedding_dimension: 4,
        })
        .expect("open store");
        store
            .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
            .expect("migrate store");
        store
    }

    fn block(content: &str, kind: BlockKind) -> CoreBlock {
        CoreBlock {
            identity: Identity {
                id: Id::generate(),
                ingested_at: ts("2026-06-01T09:00:00-05:00[America/Chicago]"),
                namespace: Namespace::Agent("identity-owner".to_string()),
                expired_at: None,
            },
            stats: Stats {
                importance: 0.95,
                trust: 0.9,
                last_access: ts("2026-06-01T09:00:00-05:00[America/Chicago]"),
                access_count_recent: 1,
                referenced_count: 0,
                surprise: 0.0,
                is_pinned: false,
            },
            content: content.to_string(),
            block_kind: kind,
            sensitivity: None,
            drift_baseline: None,
            embedding: None,
            embedder_model: None,
        }
    }

    fn insert(store: &Store, block: &CoreBlock) {
        let (labels, props) = to_node(block).expect("translate");
        let mut txn = store.graph().begin_write();
        txn.mutator().create_node(labels, props).expect("create");
        txn.commit().expect("commit");
    }

    #[test]
    fn a_full_core_block_round_trips_through_the_graph() {
        let store = store();
        let mut original = block("I keep user data confidential.", BlockKind::Redline);
        original.sensitivity = Some("pii".to_string());
        original.drift_baseline = Some(serde_json::json!({
            "summary": "confidentiality stance",
            "centroid": [0.1, 0.2],
        }));
        original.embedding = Some(Embedding::new(vec![0.1, 0.2, 0.3, 0.4]).expect("embedding"));
        original.embedder_model = Some(EmbedderModel {
            family: "fake".to_string(),
            version: "1".to_string(),
            dimension: 4,
        });
        insert(&store, &original);

        let read = store
            .core_block_by_id(&original.identity.id)
            .expect("read")
            .expect("present");
        assert_eq!(read, original, "every field survives the round trip");
    }

    #[test]
    fn optional_fields_absent_round_trip_as_none() {
        let store = store();
        let original = block("I act in the user's interest.", BlockKind::Persona);
        insert(&store, &original);

        let read = store
            .core_block_by_id(&original.identity.id)
            .expect("read")
            .expect("present");
        assert_eq!(read.sensitivity, None);
        assert_eq!(read.drift_baseline, None);
        assert_eq!(read.embedding, None);
        assert_eq!(read.embedder_model, None);
        assert_eq!(read, original);
    }

    #[test]
    fn the_live_reader_orders_by_id_and_skips_expired() {
        let store = store();
        let a = block("commitment one", BlockKind::Commitment);
        let b = block("commitment two", BlockKind::Commitment);
        let mut dead = block("a retired persona", BlockKind::Persona);
        dead.identity.expired_at = Some(ts("2026-06-02T09:00:00-05:00[America/Chicago]"));
        insert(&store, &a);
        insert(&store, &b);
        insert(&store, &dead);

        let live = store.live_core_blocks().expect("scan");
        assert_eq!(live.len(), 2, "the expired block never surfaces");
        let mut expected = [a.identity.id, b.identity.id];
        expected.sort();
        assert_eq!(
            live.iter().map(|x| x.identity.id).collect::<Vec<_>>(),
            expected,
            "id-ordered for deterministic assembly"
        );
        // An expired block still resolves by id — the edit gate and the point ops
        // name a block that exists rather than claiming it does not.
        assert!(
            store
                .core_block_by_id(&dead.identity.id)
                .expect("read")
                .is_some()
        );
    }

    #[test]
    fn an_unknown_id_resolves_to_none_and_an_empty_store_scans_empty() {
        let store = store();
        assert!(
            store
                .core_block_by_id(&Id::generate())
                .expect("read")
                .is_none()
        );
        assert!(store.live_core_blocks().expect("scan").is_empty());
    }
}
