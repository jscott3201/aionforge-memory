//! Translation between a domain [`Skill`] and a selene-db node, plus the versioned
//! skill write/read surface (02 §4.4, 05; M3.T04).
//!
//! A skill is a procedure stored as data: named, monotonically versioned per name, and
//! reliability-scored. The substrate **deprecates, never deletes** — [`Store::save_skill`]
//! writes a new version node and stamps the prior active one's `deprecated_at` in one atomic
//! commit, so the full version history is retained and at most one version per name is active.
//! Each version's fields (body, capabilities, params) are immutable once written: a change is
//! a new version, never an in-place edit. The only mutations a stored skill ever takes are the
//! deprecation stamp and the outcome counters ([`Store::record_skill_outcome`]).
//!
//! Skill retrieval (problem-embedding vector + BM25 over `description`) rides the generic
//! [`SearchKind::Skill`](crate::SearchKind) search surface; the success-weighted ranking is a
//! layer-2 concern that composes those signals.

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::edges::Audit;
use aionforge_domain::ids::Id;
use aionforge_domain::nodes::forensic::AuditEvent;
use aionforge_domain::nodes::procedural::Skill;
use aionforge_domain::time::Timestamp;
use selene_core::{
    DbString, LabelDiff, LabelSet, NodeId, PropertyDiff, PropertyMap, Value, db_string,
};
use selene_graph::{RowIndex, SeleneGraph};

use crate::convert::{
    as_bool, as_content_hash, as_embedder_model, as_embedding, as_f64, as_i64, as_id, as_namespace,
    as_str, as_timestamp, as_u64, embedder_model_value, embedding_value, hash_value, id_value,
    json_from_value, json_value, key, namespace_value, string_value, timestamp_value,
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
// Skill per-kind fields (§4.4).
const NAME: &str = "name";
const VERSION: &str = "version";
const DESCRIPTION: &str = "description";
const PROBLEM_EMBEDDING: &str = "problem_embedding_v1";
const EMBEDDER_MODEL: &str = "embedder_model";
const LANGUAGE: &str = "language";
const BODY: &str = "body";
const PARAMS: &str = "params";
const PRECONDITIONS: &str = "preconditions";
const POSTCONDITIONS: &str = "postconditions";
const CAPABILITIES: &str = "capabilities";
const SUCCESS_COUNT: &str = "success_count";
const FAILURE_COUNT: &str = "failure_count";
const MEAN_LATENCY_MS: &str = "mean_latency_ms";
const SOURCE_HASH: &str = "source_hash";
const LAST_SUCCESS_AT: &str = "last_success_at";
const LAST_FAILURE_AT: &str = "last_failure_at";
const DEPRECATED_AT: &str = "deprecated_at";
const INDUCED: &str = "induced";

/// The selene-db node label for a skill (mirrors [`Skill::LABEL`]).
pub(crate) fn label() -> Result<LabelSet, StoreError> {
    Ok(LabelSet::single(db_string(Skill::LABEL)?))
}

/// Translate a [`Skill`] into the `(labels, properties)` pair for `create_node`.
pub(crate) fn to_node(skill: &Skill) -> Result<(LabelSet, PropertyMap), StoreError> {
    let mut pairs: Vec<(DbString, Value)> = Vec::with_capacity(30);

    // Identity block.
    pairs.push((key(ID)?, id_value(&skill.identity.id)?));
    pairs.push((
        key(INGESTED_AT)?,
        timestamp_value(&skill.identity.ingested_at),
    ));
    pairs.push((key(NAMESPACE)?, namespace_value(&skill.identity.namespace)?));
    if let Some(expired_at) = &skill.identity.expired_at {
        pairs.push((key(EXPIRED_AT)?, timestamp_value(expired_at)));
    }

    // Stats block.
    pairs.push((key(IMPORTANCE)?, Value::Float(skill.stats.importance)));
    pairs.push((key(TRUST)?, Value::Float(skill.stats.trust)));
    pairs.push((key(LAST_ACCESS)?, timestamp_value(&skill.stats.last_access)));
    pairs.push((
        key(ACCESS_COUNT_RECENT)?,
        Value::Uint(skill.stats.access_count_recent),
    ));
    pairs.push((
        key(REFERENCED_COUNT)?,
        Value::Uint(skill.stats.referenced_count),
    ));
    pairs.push((key(SURPRISE)?, Value::Float(skill.stats.surprise)));
    pairs.push((key(IS_PINNED)?, Value::Bool(skill.stats.is_pinned)));

    // Per-kind fields.
    pairs.push((key(NAME)?, string_value(&skill.name)?));
    pairs.push((key(VERSION)?, Value::Int(skill.version)));
    pairs.push((key(DESCRIPTION)?, string_value(&skill.description)?));
    if let Some(embedding) = &skill.problem_embedding {
        pairs.push((key(PROBLEM_EMBEDDING)?, embedding_value(embedding)?));
    }
    if let Some(model) = &skill.embedder_model {
        pairs.push((key(EMBEDDER_MODEL)?, embedder_model_value(model)?));
    }
    pairs.push((key(LANGUAGE)?, string_value(&skill.language)?));
    pairs.push((key(BODY)?, string_value(&skill.body)?));
    pairs.push((key(PARAMS)?, json_value(&skill.params)?));
    if let Some(preconditions) = &skill.preconditions {
        pairs.push((key(PRECONDITIONS)?, json_value(preconditions)?));
    }
    if let Some(postconditions) = &skill.postconditions {
        pairs.push((key(POSTCONDITIONS)?, json_value(postconditions)?));
    }
    if !skill.capabilities.is_empty() {
        let items = skill
            .capabilities
            .iter()
            .map(|c| string_value(c))
            .collect::<Result<Vec<_>, _>>()?;
        pairs.push((key(CAPABILITIES)?, Value::List(items)));
    }
    pairs.push((key(SUCCESS_COUNT)?, Value::Uint(skill.success_count)));
    pairs.push((key(FAILURE_COUNT)?, Value::Uint(skill.failure_count)));
    if let Some(latency) = skill.mean_latency_ms {
        pairs.push((key(MEAN_LATENCY_MS)?, Value::Float(latency)));
    }
    pairs.push((key(SOURCE_HASH)?, hash_value(&skill.source_hash)?));
    if let Some(at) = &skill.last_success_at {
        pairs.push((key(LAST_SUCCESS_AT)?, timestamp_value(at)));
    }
    if let Some(at) = &skill.last_failure_at {
        pairs.push((key(LAST_FAILURE_AT)?, timestamp_value(at)));
    }
    if let Some(at) = &skill.deprecated_at {
        pairs.push((key(DEPRECATED_AT)?, timestamp_value(at)));
    }
    pairs.push((key(INDUCED)?, Value::Bool(skill.induced)));

    Ok((label()?, PropertyMap::from_pairs(pairs)?))
}

/// Reconstruct a [`Skill`] from a node's stored property map.
pub(crate) fn from_properties(props: &PropertyMap) -> Result<Skill, StoreError> {
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
    let capabilities = match get(CAPABILITIES)? {
        Some(Value::List(items)) => items
            .iter()
            .map(|v| Ok(as_str(v)?.to_string()))
            .collect::<Result<Vec<_>, StoreError>>()?,
        _ => Vec::new(),
    };

    Ok(Skill {
        identity,
        stats,
        name: as_str(require(NAME)?)?.to_string(),
        version: as_i64(require(VERSION)?)?,
        description: as_str(require(DESCRIPTION)?)?.to_string(),
        problem_embedding: get(PROBLEM_EMBEDDING)?.map(as_embedding).transpose()?,
        embedder_model: get(EMBEDDER_MODEL)?.map(as_embedder_model).transpose()?,
        language: as_str(require(LANGUAGE)?)?.to_string(),
        body: as_str(require(BODY)?)?.to_string(),
        params: json_from_value(require(PARAMS)?)?,
        preconditions: get(PRECONDITIONS)?.map(json_from_value).transpose()?,
        postconditions: get(POSTCONDITIONS)?.map(json_from_value).transpose()?,
        capabilities,
        success_count: as_u64(require(SUCCESS_COUNT)?)?,
        failure_count: as_u64(require(FAILURE_COUNT)?)?,
        mean_latency_ms: get(MEAN_LATENCY_MS)?.map(as_f64).transpose()?,
        source_hash: as_content_hash(require(SOURCE_HASH)?)?,
        last_success_at: get(LAST_SUCCESS_AT)?.map(as_timestamp).transpose()?,
        last_failure_at: get(LAST_FAILURE_AT)?.map(as_timestamp).transpose()?,
        deprecated_at: get(DEPRECATED_AT)?.map(as_timestamp).transpose()?,
        induced: as_bool(require(INDUCED)?)?,
    })
}

/// The node ids of every `Skill` with this `name` (`name` is scalar-indexed, so a probe).
fn skill_nodes_by_name(snapshot: &SeleneGraph, name: &str) -> Result<Vec<NodeId>, StoreError> {
    let label = db_string(Skill::LABEL)?;
    let prop = db_string(NAME)?;
    let value = string_value(name)?;
    let Some(rows) = snapshot.nodes_with_property_eq(&label, &prop, &value) else {
        return Ok(Vec::new());
    };
    Ok(rows
        .iter()
        .filter_map(|row| snapshot.node_id_for_row(RowIndex::new(row)))
        .collect())
}

impl Store {
    /// Save a new skill version through the single write funnel, returning its node id (05).
    ///
    /// One atomic commit, deprecate-never-delete: the new `Skill` node is created; if
    /// `deprecate_prior` is given, that node's `deprecated_at` is stamped with the new
    /// version's `ingested_at` (the supersession instant), so at most one version per name is
    /// ever active. Each `audit` event is written and wired `AuditEvent -AUDIT-> Skill` to the
    /// new version — the version-diff provenance (05): typically a `SkillSave`, plus both a
    /// `SkillDeprecate` and a `SkillVersionDiff` when a prior version was superseded. Each event
    /// carries its own `subject_id` (the version it describes); the `AUDIT` edges all anchor to
    /// the new node so a version-diff traversal starts from one place. Nothing is published if
    /// any step fails (the transaction rolls back).
    ///
    /// The caller (L2) must guarantee version monotonicity per name and construct the complete
    /// audit set; this surface is the mechanical, atomic persistence underneath those policies.
    ///
    /// # Errors
    /// Returns [`StoreError`] if translation, a mutation, or the commit fails.
    pub fn save_skill(
        &self,
        skill: &Skill,
        deprecate_prior: Option<NodeId>,
        audits: &[AuditEvent],
    ) -> Result<NodeId, StoreError> {
        let (labels, props) = to_node(skill)?;
        let audit_label = db_string(Audit::LABEL)?;
        let mut txn = self.graph().begin_write();
        let skill_node = {
            let mut mutator = txn.mutator();
            let skill_node = mutator.create_node(labels, props)?;
            if let Some(prior) = deprecate_prior {
                mutator.update_node(
                    prior,
                    LabelDiff::new([], [])?,
                    PropertyDiff::new(
                        [(
                            db_string(DEPRECATED_AT)?,
                            timestamp_value(&skill.identity.ingested_at),
                        )],
                        [],
                    )?,
                )?;
            }
            for event in audits {
                let (audit_labels, audit_props) = crate::audit::to_node(event)?;
                let audit_node = mutator.create_node(audit_labels, audit_props)?;
                mutator.create_edge(
                    audit_label.clone(),
                    audit_node,
                    skill_node,
                    PropertyMap::from_pairs(Vec::new())?,
                )?;
            }
            skill_node
        };
        txn.commit()?;
        Ok(skill_node)
    }

    /// Record a success/failure outcome against a skill, bumping its counters (05).
    ///
    /// One atomic commit that increments `success_count` or `failure_count` and stamps the
    /// matching `last_success_at` / `last_failure_at`. The body, capabilities, and version are
    /// untouched — a stored version's procedure is immutable; only its reliability stats move.
    ///
    /// # Errors
    /// Returns [`StoreError`] if `skill` is not a live node, a counter would overflow, or a
    /// mutation or the commit fails.
    pub fn record_skill_outcome(
        &self,
        skill: NodeId,
        success: bool,
        at: &Timestamp,
    ) -> Result<(), StoreError> {
        let counter = if success {
            SUCCESS_COUNT
        } else {
            FAILURE_COUNT
        };
        let last_at = if success {
            LAST_SUCCESS_AT
        } else {
            LAST_FAILURE_AT
        };
        let mut txn = self.graph().begin_write();
        {
            let mut mutator = txn.mutator();
            // Read the current counter, then drop the read borrow before mutating.
            let current = {
                let props = mutator.read().node_properties(skill).ok_or_else(|| {
                    StoreError::decode("skill node not found for outcome".to_string())
                })?;
                let value = props.get(&db_string(counter)?).ok_or_else(|| {
                    StoreError::decode(format!("skill is missing required property `{counter}`"))
                })?;
                as_u64(value)?
            };
            let next = current
                .checked_add(1)
                .ok_or_else(|| StoreError::invariant(format!("skill {counter} overflow")))?;
            mutator.update_node(
                skill,
                LabelDiff::new([], [])?,
                PropertyDiff::new(
                    [
                        (db_string(counter)?, Value::Uint(next)),
                        (db_string(last_at)?, timestamp_value(at)),
                    ],
                    [],
                )?,
            )?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Read a skill back by its node id from a fresh snapshot.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the stored data cannot be decoded into a [`Skill`].
    pub fn skill_by_node_id(&self, id: NodeId) -> Result<Option<Skill>, StoreError> {
        let snapshot = self.graph().read();
        match snapshot.node_properties(id) {
            Some(props) => Ok(Some(from_properties(props)?)),
            None => Ok(None),
        }
    }

    /// Read a skill back by its domain id from a fresh snapshot (`id` is `UNIQUE`, so a probe).
    ///
    /// # Errors
    /// Returns [`StoreError`] if the lookup fails or the stored data cannot be decoded.
    pub fn skill_by_id(&self, id: &Id) -> Result<Option<Skill>, StoreError> {
        let snapshot = self.graph().read();
        let label = db_string(Skill::LABEL)?;
        let prop = db_string(ID)?;
        let value = id_value(id)?;
        let Some(rows) = snapshot.nodes_with_property_eq(&label, &prop, &value) else {
            return Ok(None);
        };
        for row in rows.iter() {
            let Some(node) = snapshot.node_id_for_row(RowIndex::new(row)) else {
                continue;
            };
            if let Some(props) = snapshot.node_properties(node) {
                return Ok(Some(from_properties(props)?));
            }
        }
        Ok(None)
    }

    /// The active (non-deprecated) version of the skill named `name`, with its node id.
    ///
    /// At most one version per name is active — the deprecate-on-save protocol in
    /// [`Store::save_skill`] maintains the invariant — so this returns that unique version and
    /// its node id, or `None` if no live, non-deprecated version exists. Repeated calls for the
    /// same name return the same `(NodeId, Skill)` pair.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the lookup fails or a stored skill cannot be decoded.
    pub fn active_skill(&self, name: &str) -> Result<Option<(NodeId, Skill)>, StoreError> {
        let snapshot = self.graph().read();
        for node in skill_nodes_by_name(&snapshot, name)? {
            if let Some(props) = snapshot.node_properties(node) {
                let skill = from_properties(props)?;
                if skill.deprecated_at.is_none() {
                    return Ok(Some((node, skill)));
                }
            }
        }
        Ok(None)
    }

    /// Every version of the skill named `name`, in ascending `version` order.
    ///
    /// The full retained history — active and deprecated — since the substrate never deletes.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the lookup fails or a stored skill cannot be decoded.
    pub fn skill_versions(&self, name: &str) -> Result<Vec<Skill>, StoreError> {
        let snapshot = self.graph().read();
        let mut skills = Vec::new();
        for node in skill_nodes_by_name(&snapshot, name)? {
            if let Some(props) = snapshot.node_properties(node) {
                skills.push(from_properties(props)?);
            }
        }
        skills.sort_by_key(|skill| skill.version);
        Ok(skills)
    }
}
