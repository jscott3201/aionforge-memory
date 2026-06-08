//! Translation between a domain [`BadPattern`] and a selene-db node, plus the failure-mode
//! write/read surface linking a skill to the patterns observed running it (02 §4.5, 05; M3.T05).
//!
//! A bad pattern is negative procedural memory: a recorded failure mode an agent should avoid
//! reusing. It is immutable once written — a failure is a new pattern, never an edit — and is
//! wired to the skill it was observed against by a `HAS_FAILURE` edge. Saving one is itself a
//! failure outcome, so [`Store::save_bad_pattern`] bumps the skill's failure counter in the same
//! atomic commit, sharing the counter logic with [`Store::record_skill_outcome`].
//!
//! Bad patterns carry only a problem-content embedding (no BM25 index), so they are retrieved by
//! vector similarity; the layer-2 ranking judges a pattern's relevance to the current problem
//! from that embedding.

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::edges::HasFailure;
use aionforge_domain::nodes::procedural::BadPattern;
use selene_core::{DbString, LabelSet, NodeId, PropertyMap, Value, db_string};

use crate::convert::{
    as_bool, as_embedder_model, as_embedding, as_f64, as_id, as_namespace, as_str, as_timestamp,
    as_u64, embedder_model_value, embedding_value, id_value, key, namespace_value, string_value,
    timestamp_value,
};
use crate::error::StoreError;
use crate::skill::bump_skill_outcome;
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
// BadPattern per-kind fields (§4.5).
const DESCRIPTION: &str = "description";
const EMBEDDING: &str = "embedding_v1";
const EMBEDDER_MODEL: &str = "embedder_model";
const OBSERVED_AT: &str = "observed_at";

/// The selene-db node label for a bad pattern (mirrors [`BadPattern::LABEL`]).
pub(crate) fn label() -> Result<LabelSet, StoreError> {
    Ok(LabelSet::single(db_string(BadPattern::LABEL)?))
}

/// Translate a [`BadPattern`] into the `(labels, properties)` pair for `create_node`.
pub(crate) fn to_node(pattern: &BadPattern) -> Result<(LabelSet, PropertyMap), StoreError> {
    let mut pairs: Vec<(DbString, Value)> = Vec::with_capacity(15);

    // Identity block.
    pairs.push((key(ID)?, id_value(&pattern.identity.id)?));
    pairs.push((
        key(INGESTED_AT)?,
        timestamp_value(&pattern.identity.ingested_at),
    ));
    pairs.push((
        key(NAMESPACE)?,
        namespace_value(&pattern.identity.namespace)?,
    ));
    if let Some(expired_at) = &pattern.identity.expired_at {
        pairs.push((key(EXPIRED_AT)?, timestamp_value(expired_at)));
    }

    // Stats block.
    pairs.push((key(IMPORTANCE)?, Value::Float(pattern.stats.importance)));
    pairs.push((key(TRUST)?, Value::Float(pattern.stats.trust)));
    pairs.push((
        key(LAST_ACCESS)?,
        timestamp_value(&pattern.stats.last_access),
    ));
    pairs.push((
        key(ACCESS_COUNT_RECENT)?,
        Value::Uint(pattern.stats.access_count_recent),
    ));
    pairs.push((
        key(REFERENCED_COUNT)?,
        Value::Uint(pattern.stats.referenced_count),
    ));
    pairs.push((key(SURPRISE)?, Value::Float(pattern.stats.surprise)));
    pairs.push((key(IS_PINNED)?, Value::Bool(pattern.stats.is_pinned)));

    // Per-kind fields.
    pairs.push((key(DESCRIPTION)?, string_value(&pattern.description)?));
    if let Some(embedding) = &pattern.embedding {
        pairs.push((key(EMBEDDING)?, embedding_value(embedding)?));
    }
    if let Some(model) = &pattern.embedder_model {
        pairs.push((key(EMBEDDER_MODEL)?, embedder_model_value(model)?));
    }
    pairs.push((key(OBSERVED_AT)?, timestamp_value(&pattern.observed_at)));

    Ok((label()?, PropertyMap::from_pairs(pairs)?))
}

/// Reconstruct a [`BadPattern`] from a node's stored property map.
pub(crate) fn from_properties(props: &PropertyMap) -> Result<BadPattern, StoreError> {
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

    Ok(BadPattern {
        identity,
        stats,
        description: as_str(require(DESCRIPTION)?)?.to_string(),
        embedding: get(EMBEDDING)?.map(as_embedding).transpose()?,
        embedder_model: get(EMBEDDER_MODEL)?.map(as_embedder_model).transpose()?,
        observed_at: as_timestamp(require(OBSERVED_AT)?)?,
    })
}

impl Store {
    /// Save a bad pattern and wire it to the skill it was observed against (05; M3.T05).
    ///
    /// One atomic commit: the `BadPattern` node is created, a `Skill -HAS_FAILURE-> BadPattern`
    /// edge is wired carrying the observation instant, and the skill's failure counter is bumped
    /// (the same stat a plain failure outcome moves) — recording a failure mode *is* a failure.
    /// The bad pattern's `observed_at` is the single source of the edge time and the counter
    /// stamp, so all three agree. Nothing is published if any step fails.
    ///
    /// # Errors
    /// Returns [`StoreError`] if `skill` is not a live skill node, translation fails, a mutation
    /// fails, or the commit fails.
    pub fn save_bad_pattern(
        &self,
        pattern: &BadPattern,
        skill: NodeId,
    ) -> Result<NodeId, StoreError> {
        let (labels, props) = to_node(pattern)?;
        let edge_label = db_string(HasFailure::LABEL)?;
        let edge_props = PropertyMap::from_pairs(vec![(
            key(OBSERVED_AT)?,
            timestamp_value(&pattern.observed_at),
        )])?;
        let mut txn = self.graph().begin_write();
        let pattern_node = {
            let mut mutator = txn.mutator();
            // Bump the failure counter first: this reads the skill (fails closed if it is not a
            // live skill node) before anything is created, so a bad id creates nothing.
            bump_skill_outcome(&mut mutator, skill, false, &pattern.observed_at)?;
            let pattern_node = mutator.create_node(labels, props)?;
            mutator.create_edge(edge_label, skill, pattern_node, edge_props)?;
            pattern_node
        };
        txn.commit()?;
        Ok(pattern_node)
    }

    /// Read a bad pattern back by its node id from a fresh snapshot.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the stored data cannot be decoded into a [`BadPattern`].
    pub fn bad_pattern_by_node_id(&self, id: NodeId) -> Result<Option<BadPattern>, StoreError> {
        let snapshot = self.graph().read();
        match snapshot.node_properties(id) {
            Some(props) => Ok(Some(from_properties(props)?)),
            None => Ok(None),
        }
    }

    /// Every bad pattern linked to `skill` via `HAS_FAILURE`, in ascending id order (05; M3.T05).
    ///
    /// The skill's recorded failure modes, gathered by one outgoing-edge traversal. Ordered by id
    /// so the result is deterministic; the layer-2 ranking re-orders by relevance to a query. A
    /// linked node that carries no stored properties is skipped — the closed graph guarantees an
    /// edge endpoint exists, so this only guards against an impossible inconsistency rather than a
    /// real case.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a linked pattern's stored properties cannot be decoded into a
    /// [`BadPattern`].
    pub fn bad_patterns_for_skill(&self, skill: NodeId) -> Result<Vec<BadPattern>, StoreError> {
        let snapshot = self.graph().read();
        let edge_label = db_string(HasFailure::LABEL)?;
        let Some(adjacency) = snapshot.outgoing_edges(skill) else {
            return Ok(Vec::new());
        };
        let mut patterns = Vec::new();
        for edge in adjacency.iter_label(&edge_label) {
            if let Some(props) = snapshot.node_properties(edge.neighbor) {
                patterns.push(from_properties(props)?);
            }
        }
        patterns.sort_by(|a, b| a.identity.id.cmp(&b.identity.id));
        Ok(patterns)
    }
}
