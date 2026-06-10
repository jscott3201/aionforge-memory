//! Native index registration (data-model §7–§8) and the dimension-consistency check
//! (§13.5).
//!
//! Indexes persist as `SchemaChange` WAL records and the engine rebuilds them from
//! primary values on recovery, so — unlike providers — they belong in the migration.
//! Vector and text indexes have no DDL, so they go through the Rust API; composite
//! indexes have no Rust wrapper, so they go through `CREATE INDEX IF NOT EXISTS` DDL.
//! Both paths are made idempotent (introspect-then-skip for vector/text, `IF NOT
//! EXISTS` for composites) so a re-run or a crash mid-migration is safe.
//!
//! selene-db now indexes `ZONED DATETIME` (a typed property index over `jiff::Zoned`,
//! ordered by absolute instant), so the §8 audit temporal composites are built here:
//! `AuditEvent.occurred_at` is the first datetime property index in the schema, and the
//! `(subject_id, occurred_at)` / `(kind, occurred_at)` composites order a subject's (or
//! a kind's) audit history at the index for the M4.T06 readers. Other §8 timestamp
//! composites that no reader needs yet are still omitted.

use selene_core::db_string;
use selene_graph::{HnswIndexConfig, TypedIndexKind, VectorIndexConfig, VectorIndexKind};

use crate::catalog::NODE_TYPES;
use crate::error::StoreError;
use crate::gql::BoundQuery;
use crate::store::Store;

/// `(label, property)` for each embedding vector index (§7). HNSW + cosine.
const VECTOR_INDEXES: &[(&str, &str)] = &[
    ("Episode", "embedding_v1"),
    ("Fact", "embedding_v1"),
    ("Entity", "embedding_v1"),
    ("Skill", "problem_embedding_v1"),
    ("BadPattern", "embedding_v1"),
    ("Note", "embedding_v1"),
    ("CoreBlock", "embedding_v1"),
];

/// `(label, property)` for each maintained BM25 text index (§8).
const TEXT_INDEXES: &[(&str, &str)] = &[
    ("Episode", "content"),
    ("Fact", "statement"),
    ("Entity", "canonical_name"),
    ("Skill", "description"),
    ("Note", "content"),
];

/// `(label, property)` for the per-kind `INDEXED` scalar fields (§4/§8). `namespace`
/// is indexed on every kind (§11) and added separately, so it is not repeated here.
/// Each entry declares its own column type via [`TypedIndexKind`] (string, UUID, and —
/// for `AuditEvent.occurred_at` — zoned datetime).
const SCALAR_INDEXES: &[(&str, &str, TypedIndexKind)] = &[
    ("Episode", "role", TypedIndexKind::String),
    ("Episode", "agent_id", TypedIndexKind::Uuid),
    ("Episode", "session_id", TypedIndexKind::Uuid),
    ("Episode", "content_hash", TypedIndexKind::String),
    ("Episode", "consolidation_state", TypedIndexKind::String),
    ("Episode", "id", TypedIndexKind::Uuid),
    ("Fact", "subject_id", TypedIndexKind::Uuid),
    ("Fact", "predicate", TypedIndexKind::String),
    ("Fact", "status", TypedIndexKind::String),
    ("Fact", "object_entity_id", TypedIndexKind::Uuid),
    ("Fact", "id", TypedIndexKind::Uuid),
    // `id` is indexed on every Stats-bearing kind (`Episode`, `Fact`, `Entity`, `Note`, `Skill`,
    // `BadPattern`, `CoreBlock`) plus `AuditEvent` and `Agent` — not on every kind.
    // Consolidation resolves an already-canonical subject entity's `NodeId` by its
    // domain id inside the flip txn when it wires the `ABOUT`/`MENTIONS` edges (M2.T04); it dedups
    // a content-addressed summary `Note` by id so replaying an episode never writes a second copy
    // (M2.T06); and it dedups a content-addressed `AuditEvent` by id for the same replay reason
    // (M2.T04 audit determinism). `Skill` is addressed by domain id at the procedural contract
    // (`ProceduralMemory::record_outcome(skill_id: Id)` and the by-id reads); L2 bridges that to
    // L0's node-keyed `record_skill_outcome` / reads via `skill_by_id`, so the id probe must be
    // indexed (M3.T04). `Agent` is addressed by domain id when provenance verification resolves a
    // writer's public key (`agent_by_id`, M4.T03) — the DDL `UNIQUE` constraint does not back the
    // scalar-equality probe, so the index is declared here. `Episode` is addressed by domain id by
    // the signed-write collision pre-check (`episode_exists`, M4.T03): a signed write adopts a
    // host-supplied subject id as its episode id, and `nodes_with_property_eq` returns `None`
    // (read as "absent") without an index, so the pre-check would silently no-op without it.
    // (Episode-id uniqueness itself is guaranteed by the `Episode.id UNIQUE` DDL at commit; the
    // index is what lets the pre-check reject a reused id cleanly, with an audit and without a
    // wasted embed, before the commit would fail.) `Fact` is addressed by domain id by quorum
    // promotion (06 §4, M4.T04): a promoted global copy takes a content-addressed,
    // namespace-leading id (`global|{team_fact_id}|promoted`), and `promote_fact` probes that id
    // (`fact_node_by_id`) to stay idempotent — on a replay it finds the existing global node and
    // writes no second one. As with `Episode`, the `Fact.id UNIQUE` DDL is the commit-time
    // backstop and `nodes_with_property_eq` returns `None` (read as "absent") without an index, so
    // the probe needs the index to mean anything. `BadPattern` and `CoreBlock` are addressed by
    // domain id by the forgetting point-op resolver (`memory_by_id`, M5.T02), which must *find* a
    // protected memory to refuse it by name — without the index the probe reads "absent" and a
    // point op on identity memory would misreport "not found". Kinds outside this set (`Session`,
    // `ProvenanceRecord`, `Promotion`) are reached by node id directly, so they need no id index.
    ("Entity", "id", TypedIndexKind::Uuid),
    ("Entity", "canonical_name", TypedIndexKind::String),
    ("Entity", "type", TypedIndexKind::String),
    ("Note", "id", TypedIndexKind::Uuid),
    ("Skill", "id", TypedIndexKind::Uuid),
    ("Skill", "name", TypedIndexKind::String),
    ("Skill", "source_hash", TypedIndexKind::String),
    ("Note", "derived_from_episode", TypedIndexKind::Uuid),
    ("BadPattern", "id", TypedIndexKind::Uuid),
    ("CoreBlock", "id", TypedIndexKind::Uuid),
    ("CoreBlock", "block_kind", TypedIndexKind::String),
    ("Agent", "id", TypedIndexKind::Uuid),
    ("Agent", "status", TypedIndexKind::String),
    ("Session", "owner_agent_id", TypedIndexKind::Uuid),
    ("ProvenanceRecord", "subject_id", TypedIndexKind::Uuid),
    ("ProvenanceRecord", "writer_agent_id", TypedIndexKind::Uuid),
    ("AuditEvent", "id", TypedIndexKind::Uuid),
    ("AuditEvent", "kind", TypedIndexKind::String),
    ("AuditEvent", "subject_id", TypedIndexKind::Uuid),
    ("AuditEvent", "actor_id", TypedIndexKind::Uuid),
    ("AuditEvent", "occurred_at", TypedIndexKind::ZonedDateTime),
    ("Promotion", "candidate_fact_id", TypedIndexKind::Uuid),
    ("Promotion", "status", TypedIndexKind::String),
];

/// Composite indexes (§8). DDL-only — no Rust wrapper. The `AuditEvent` temporal
/// composites order a subject's (or a kind's) audit history by `occurred_at` at the
/// index, so the by-subject and by-kind readers scan in instant order without a
/// sort-after-scan. Other §8 timestamp composites that no reader needs yet are omitted.
const COMPOSITE_INDEXES: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS cidx_fact_subject_predicate ON :Fact(subject_id, predicate)",
    "CREATE INDEX IF NOT EXISTS cidx_fact_subject_status ON :Fact(subject_id, status)",
    "CREATE INDEX IF NOT EXISTS cidx_skill_name_version ON :Skill(name, version)",
    "CREATE INDEX IF NOT EXISTS cidx_audit_subject_occurred ON :AuditEvent(subject_id, occurred_at)",
    "CREATE INDEX IF NOT EXISTS cidx_audit_kind_occurred ON :AuditEvent(kind, occurred_at)",
];

/// A registered vector index, for inventory and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorIndexInfo {
    /// The indexed node label.
    pub label: String,
    /// The indexed vector property.
    pub property: String,
    /// The index kind (e.g. `HnswCosine`).
    pub kind: String,
    /// The pinned dimension.
    pub dimension: u32,
    /// The catalog name, if one was given.
    pub name: Option<String>,
}

impl Store {
    /// Register every §7–§8 index, idempotently. Called from the migration.
    pub(crate) fn register_indexes(&self, embedding_dimension: u32) -> Result<(), StoreError> {
        self.register_vector_indexes(embedding_dimension)?;
        self.register_text_indexes()?;
        self.register_property_indexes()?;
        self.register_composite_indexes()?;
        Ok(())
    }

    fn register_vector_indexes(&self, dimension: u32) -> Result<(), StoreError> {
        let config = VectorIndexConfig::hnsw(HnswIndexConfig::DEFAULT);
        for &(label, property) in VECTOR_INDEXES {
            if self.vector_index_exists(label, property) {
                continue;
            }
            let name = db_string(&format!("vec_{label}_{property}"))?;
            self.graph().create_vector_index_named_with_configs(
                db_string(label)?,
                db_string(property)?,
                VectorIndexKind::HnswCosine,
                dimension,
                Some(name),
                config,
            )?;
        }
        Ok(())
    }

    fn register_text_indexes(&self) -> Result<(), StoreError> {
        for &(label, property) in TEXT_INDEXES {
            if self.text_index_exists(label, property) {
                continue;
            }
            let name = db_string(&format!("txt_{label}_{property}"))?;
            self.graph().create_text_index_named(
                db_string(label)?,
                db_string(property)?,
                Some(name),
            )?;
        }
        Ok(())
    }

    fn register_property_indexes(&self) -> Result<(), StoreError> {
        // namespace is indexed on every kind (§11).
        for type_ddl in NODE_TYPES {
            self.ensure_property_index(type_ddl.name, "namespace", TypedIndexKind::String)?;
        }
        for &(label, property, kind) in SCALAR_INDEXES {
            self.ensure_property_index(label, property, kind)?;
        }
        Ok(())
    }

    fn ensure_property_index(
        &self,
        label: &str,
        property: &str,
        kind: TypedIndexKind,
    ) -> Result<(), StoreError> {
        if self.property_index_exists(label, property) {
            return Ok(());
        }
        let name = db_string(&format!("pidx_{label}_{property}"))?;
        self.graph().create_property_index_named(
            db_string(label)?,
            db_string(property)?,
            kind,
            Some(name),
        )?;
        Ok(())
    }

    fn register_composite_indexes(&self) -> Result<(), StoreError> {
        for ddl in COMPOSITE_INDEXES {
            self.execute(&BoundQuery::new(*ddl))?;
        }
        Ok(())
    }

    fn vector_index_exists(&self, label: &str, property: &str) -> bool {
        self.graph()
            .read()
            .iter_vector_index_entries()
            .any(|(l, p, ..)| l.as_str() == label && p.as_str() == property)
    }

    fn text_index_exists(&self, label: &str, property: &str) -> bool {
        self.graph()
            .read()
            .iter_text_index_entries()
            .any(|(l, p, ..)| l.as_str() == label && p.as_str() == property)
    }

    fn property_index_exists(&self, label: &str, property: &str) -> bool {
        self.graph()
            .read()
            .iter_property_index_entries()
            .any(|(l, p, ..)| l.as_str() == label && p.as_str() == property)
    }

    /// Assert every vector index's dimension equals `embedder_dimension` (§13.5).
    ///
    /// The engine has no startup dimension scan — it validates per-mutation — so this
    /// is the loud-at-boot check the spec requires.
    ///
    /// # Errors
    /// Returns [`StoreError`] naming the first index whose dimension disagrees.
    pub fn dimension_consistency_check(&self, embedder_dimension: u32) -> Result<(), StoreError> {
        for (label, property, _kind, dimension, ..) in
            self.graph().read().iter_vector_index_entries()
        {
            if dimension != embedder_dimension {
                return Err(StoreError::decode(format!(
                    "vector index {}.{} has dimension {dimension} but the embedder dimension is {embedder_dimension}",
                    label.as_str(),
                    property.as_str(),
                )));
            }
        }
        Ok(())
    }

    /// The registered vector indexes.
    #[must_use]
    pub fn vector_indexes(&self) -> Vec<VectorIndexInfo> {
        self.graph()
            .read()
            .iter_vector_index_entries()
            .map(
                |(label, property, kind, dimension, _hnsw, _ivf, name)| VectorIndexInfo {
                    label: label.as_str().to_owned(),
                    property: property.as_str().to_owned(),
                    kind: format!("{kind:?}"),
                    dimension,
                    name: name.map(|name| name.as_str().to_owned()),
                },
            )
            .collect()
    }

    /// The registered text indexes as `(label, property)`.
    #[must_use]
    pub fn text_indexes(&self) -> Vec<(String, String)> {
        self.graph()
            .read()
            .iter_text_index_entries()
            .map(|(label, property, ..)| (label.as_str().to_owned(), property.as_str().to_owned()))
            .collect()
    }

    /// The registered scalar property indexes as `(label, property)`.
    #[must_use]
    pub fn property_indexes(&self) -> Vec<(String, String)> {
        self.graph()
            .read()
            .iter_property_index_entries()
            .map(|(label, property, ..)| (label.as_str().to_owned(), property.as_str().to_owned()))
            .collect()
    }

    /// The registered composite indexes as `(label, [property, …])`.
    #[must_use]
    pub fn composite_indexes(&self) -> Vec<(String, Vec<String>)> {
        self.graph()
            .read()
            .iter_composite_property_index_entries()
            .map(|(label, properties, ..)| {
                (
                    label.as_str().to_owned(),
                    properties
                        .iter()
                        .map(|property| property.as_str().to_owned())
                        .collect(),
                )
            })
            .collect()
    }
}
