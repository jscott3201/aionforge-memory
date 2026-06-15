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
use serde::{Deserialize, Serialize};

use crate::catalog::NODE_TYPES;
use crate::error::StoreError;
use crate::gql::BoundQuery;
use crate::store::Store;

/// `(label, property, kind)` for each embedding vector index (§7). All cosine.
///
/// The kind column is the single source of truth: [`Store::register_vector_indexes`]
/// creates each index with it, and the doctor derives its per-index kind expectation
/// from this same table (`doctor.rs`). A kind change here therefore moves the
/// registration and the health check in lockstep — there is no second constant to keep
/// in sync, which is what previously made any non-`HnswCosine` kind trip a spurious
/// doctor mismatch.
pub(crate) const VECTOR_INDEXES: &[(&str, &str, VectorIndexKind)] = &[
    // Every embedding corpus takes selene 1.2's TurboQuant cosine index: a 4-bit-compressed
    // candidate index (~8x less RAM than full f32 at 3072-dim) that preselects, then EXACTLY
    // reranks survivors against the stored full-precision vectors — so cosine accuracy on the
    // rerank is preserved while the candidate index shrinks. It is the default for all kinds
    // because the rerank keeps recall intact even on the small corpora: with a 512-wide
    // candidate sweep, any corpus under ~512 rows has every row clear the sweep and get an
    // exact full-precision rerank (recall 1.0), so the smaller kinds pay no accuracy cost for
    // the RAM win. Cosine-only and opt-in (1.2's default kind is ivf_cosine), so the kind is
    // named explicitly here.
    ("Episode", "embedding_v1", VectorIndexKind::TurboQuantCosine),
    ("Fact", "embedding_v1", VectorIndexKind::TurboQuantCosine),
    ("Entity", "embedding_v1", VectorIndexKind::TurboQuantCosine),
    (
        "Skill",
        "problem_embedding_v1",
        VectorIndexKind::TurboQuantCosine,
    ),
    (
        "BadPattern",
        "embedding_v1",
        VectorIndexKind::TurboQuantCosine,
    ),
    ("Note", "embedding_v1", VectorIndexKind::TurboQuantCosine),
    (
        "CoreBlock",
        "embedding_v1",
        VectorIndexKind::TurboQuantCosine,
    ),
];

/// `(label, property)` for each maintained BM25 text index (§8).
pub(crate) const TEXT_INDEXES: &[(&str, &str)] = &[
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
pub(crate) const SCALAR_INDEXES: &[(&str, &str, TypedIndexKind)] = &[
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
    // Work-tracking facet (work-structure design §2–§3). PR1 readers probe `parent_id` (the
    // indexed self-referential containment spine — children-of-parent is a single probe),
    // `work_status` (the status facet), and `id` (the by-id resolver). `level` (the level facet)
    // and Tag `slug` ("items tagged X") are provisioned here for the PR3 query readers and are
    // registered now so the schema migration is decided once; until those readers land they index
    // a column nothing probes yet. Without the index `nodes_with_property_eq` returns `None` (read
    // as "absent"), so any probe a future reader adds is silently wrong unless its index exists —
    // hence provisioning them with the schema rather than in a later migration.
    ("WorkItem", "id", TypedIndexKind::Uuid),
    ("WorkItem", "parent_id", TypedIndexKind::Uuid),
    ("WorkItem", "work_status", TypedIndexKind::String),
    ("WorkItem", "level", TypedIndexKind::String),
    ("Tag", "id", TypedIndexKind::Uuid),
    ("Tag", "slug", TypedIndexKind::String),
];

/// Composite indexes (§8) as `(node_label, DDL)`. DDL-only — no Rust wrapper. The label
/// is carried alongside the DDL so the registrar can skip an entry whose type the bound
/// graph does not declare (a partial recovery) without parsing the statement; on a fresh
/// migration every type is already declared, so the guard never skips there. The
/// `AuditEvent` temporal composites order a subject's (or a kind's) audit history by
/// `occurred_at` at the index, so the by-subject and by-kind readers scan in instant order
/// without a sort-after-scan. Other §8 timestamp composites that no reader needs yet are
/// omitted.
const COMPOSITE_INDEXES: &[(&str, &str)] = &[
    (
        "Fact",
        "CREATE INDEX IF NOT EXISTS cidx_fact_subject_predicate ON :Fact(subject_id, predicate)",
    ),
    (
        "Fact",
        "CREATE INDEX IF NOT EXISTS cidx_fact_subject_status ON :Fact(subject_id, status)",
    ),
    (
        "Skill",
        "CREATE INDEX IF NOT EXISTS cidx_skill_name_version ON :Skill(name, version)",
    ),
    (
        "AuditEvent",
        "CREATE INDEX IF NOT EXISTS cidx_audit_subject_occurred ON :AuditEvent(subject_id, occurred_at)",
    ),
    (
        "AuditEvent",
        "CREATE INDEX IF NOT EXISTS cidx_audit_kind_occurred ON :AuditEvent(kind, occurred_at)",
    ),
    // Orders a work item's children by `ordinal` at the index, so the by-parent reader returns
    // siblings in declared order. The substrate probes the single `parent_id` scalar and the
    // composite serves the ordering; it is not a two-key equality lookup.
    (
        "WorkItem",
        "CREATE INDEX IF NOT EXISTS cidx_workitem_parent_ordinal ON :WorkItem(parent_id, ordinal)",
    ),
];

/// A registered vector index, for inventory and tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// A focused, operator-facing projection of one vector index's memory and health
/// profile, from selene 1.2's `VectorIndexMemoryUsage`.
///
/// The engine accounts ~40 columns; this surfaces the fields that drive capacity
/// planning and make the TurboQuant compression win measurable. `estimated_index_bytes`
/// counts the index-owned structures (the compressed candidate index for TurboQuant —
/// the number that shrinks vs a full-precision HNSW graph), while
/// `estimated_reachable_bytes` adds the full-precision vector component bytes the index
/// references, as an upper bound. The gap between them is the compression story.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorIndexStats {
    /// The indexed node label.
    pub label: String,
    /// The indexed vector property.
    pub property: String,
    /// The index kind (e.g. `TurboQuantCosine`), Debug-rendered.
    pub kind: String,
    /// Live rows currently admitted to the index.
    pub indexed_rows: u64,
    /// Estimated heap bytes owned by the derived index structures, excluding the
    /// full-precision vector components shared with the primary graph.
    pub estimated_index_bytes: usize,
    /// Upper-bound estimate including the vector component bytes the index references.
    pub estimated_reachable_bytes: usize,
    /// True when this index carries a TurboQuant compressed accelerator.
    pub is_turbo_quant: bool,
    /// True when the engine recommends an IVF rebuild for accumulated drift.
    pub ivf_rebuild_recommended: bool,
}

/// One vector index whose live kind was reconciled to the catalog kind on open.
///
/// Returned by [`Store::reconcile_vector_index_kinds`] so the caller can emit one
/// observability line per drop-and-recreate (the "auto-reconcile + metric" policy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorKindReconciliation {
    /// The indexed node label.
    pub label: String,
    /// The indexed vector property.
    pub property: String,
    /// The Debug-rendered kind the index had before reconciliation.
    pub from_kind: String,
    /// The Debug-rendered catalog kind the index now has.
    pub to_kind: String,
}

/// The catalog kind declared for `(label, property)` in [`VECTOR_INDEXES`], or `None`
/// when the pair is not a declared embedding index. The catalog is the single source of
/// truth for the expected kind (mirrors `doctor::expected_vector_kind`, but returns the
/// enum for a direct, Debug-free comparison against the live index entry).
fn catalog_vector_kind(label: &str, property: &str) -> Option<VectorIndexKind> {
    VECTOR_INDEXES
        .iter()
        .find(|(declared_label, declared_property, _kind)| {
            *declared_label == label && *declared_property == property
        })
        .map(|&(_label, _property, kind)| kind)
}

impl Store {
    /// Register every §7–§8 index, idempotently. Called from the migration.
    ///
    /// Each per-class registrar returns the `(label, property)` pairs it actually
    /// created; the migration discards them (it only needs the indexes to exist), but
    /// the same registrars are the single create-if-missing path that the recovery-time
    /// [`Store::ensure_catalog_indexes`] (`reconcile.rs`) reuses and whose return value
    /// it collects — so there is exactly one creation path per class and the two callers
    /// cannot drift.
    pub(crate) fn register_indexes(&self, embedding_dimension: u32) -> Result<(), StoreError> {
        self.register_vector_indexes(embedding_dimension)?;
        self.register_text_indexes()?;
        self.register_property_indexes()?;
        self.register_composite_indexes()?;
        Ok(())
    }

    /// Create every missing catalog vector index at `dimension`, returning the
    /// `(label, property)` pairs created (empty when all already existed).
    pub(crate) fn register_vector_indexes(
        &self,
        dimension: u32,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let mut created = Vec::new();
        for &(label, property, kind) in VECTOR_INDEXES {
            // Skip a label the bound type does not declare. The migration always declares
            // every type before registering indexes, so this never skips there; it only
            // matters on recovery of a partial/drifted binding, where index creation must
            // not crash on an undeclared type — the schema/latch checks own that verdict.
            if !self.has_node_type(label) || self.vector_index_exists(label, property) {
                continue;
            }
            self.create_vector_index_for(label, property, kind, dimension)?;
            created.push((label.to_owned(), property.to_owned()));
        }
        Ok(created)
    }

    /// Create one vector index with the kind-appropriate construction config.
    ///
    /// Shared by the migration's [`Store::register_vector_indexes`] (fresh creation) and
    /// the recovery-time [`Store::reconcile_vector_index_kinds`] (drop-then-recreate), so
    /// both paths build an index identically — there is no second create path to drift.
    /// selene backfills the new index from the primary VECTOR columns, so a recreate is
    /// non-lossy.
    fn create_vector_index_for(
        &self,
        label: &str,
        property: &str,
        kind: VectorIndexKind,
        dimension: u32,
    ) -> Result<(), StoreError> {
        // selene rejects an HNSW construction config on a non-HNSW index
        // (VectorIndexInvalidHnswConfig), so attach the HNSW config only to HNSW kinds;
        // the TurboQuant/IVF cosine kinds take the empty default (their construction
        // parameters are engine-internal).
        let config = if kind.hnsw_metric().is_some() {
            VectorIndexConfig::hnsw(HnswIndexConfig::DEFAULT)
        } else {
            VectorIndexConfig::default()
        };
        let name = db_string(&format!("vec_{label}_{property}"))?;
        self.graph().create_vector_index_named_with_configs(
            db_string(label)?,
            db_string(property)?,
            kind,
            dimension,
            Some(name),
            config,
        )?;
        Ok(())
    }

    /// Reconcile each catalog vector index whose live kind disagrees with the catalog,
    /// by dropping it and recreating it at the catalog kind. selene backfills the new
    /// index from the primary VECTOR columns, so the recreate is non-lossy.
    ///
    /// This is the interim "remove the greenfield tax" step (steward backlog #7): a
    /// kind-only catalog change (e.g. `HnswCosine` -> `TurboQuantCosine`) converges on
    /// the next open instead of forcing a fresh store. It needs no new graph types, so
    /// it is safe to run on a recovered, already-migrated graph.
    ///
    /// Dimension-preserving by construction: an index whose dimension disagrees with
    /// `embedder_dimension` is LEFT ALONE, because that is the embedder-changed case — a
    /// lossy re-embed that [`Store::dimension_consistency_check`] must reject loudly,
    /// not one reconciliation should silently paper over. Callers run this BEFORE that
    /// check so the dimension guard still fires on a real embedder change.
    ///
    /// Idempotent: a second run finds no mismatches and returns an empty report. Each
    /// drop+create is its own engine commit, so a crash mid-reconcile leaves a partially
    /// converged store that the next open re-reconciles.
    ///
    /// # Errors
    /// Returns [`StoreError`] if dropping or recreating an index fails.
    pub(crate) fn reconcile_vector_index_kinds(
        &self,
        embedder_dimension: u32,
    ) -> Result<Vec<VectorKindReconciliation>, StoreError> {
        // Snapshot the mismatches BEFORE mutating: dropping an index invalidates the live
        // index iterator, so the drop loop cannot borrow it. Each tuple is
        // (label, property, catalog_kind, from_kind_debug).
        let pending: Vec<(String, String, VectorIndexKind, String)> = self
            .graph()
            .read()
            .iter_vector_index_entries()
            .filter_map(|(label, property, kind, dimension, ..)| {
                let label = label.as_str().to_owned();
                let property = property.as_str().to_owned();
                let expected = catalog_vector_kind(&label, &property)?;
                // Reconcile only a kind drift at the right dimension; skip indexes already
                // at the catalog kind and dimension-mismatched indexes (deferred to the
                // dimension check).
                (kind != expected && dimension == embedder_dimension)
                    .then(|| (label, property, expected, format!("{kind:?}")))
            })
            .collect();

        let mut reconciled = Vec::with_capacity(pending.len());
        for (label, property, to_kind, from_kind) in pending {
            // Drop, then recreate at the catalog kind. selene backfills from the primary
            // VECTOR columns, so no embedding is lost or re-embedded.
            self.graph()
                .drop_vector_index(db_string(&label)?, db_string(&property)?)?;
            self.create_vector_index_for(&label, &property, to_kind, embedder_dimension)?;
            reconciled.push(VectorKindReconciliation {
                label,
                property,
                from_kind,
                to_kind: format!("{to_kind:?}"),
            });
        }
        Ok(reconciled)
    }

    /// Create every missing catalog text index, returning the `(label, property)` pairs
    /// created (empty when all already existed).
    pub(crate) fn register_text_indexes(&self) -> Result<Vec<(String, String)>, StoreError> {
        let mut created = Vec::new();
        for &(label, property) in TEXT_INDEXES {
            // See `register_vector_indexes`: skip a label the bound type does not declare.
            if !self.has_node_type(label) || self.text_index_exists(label, property) {
                continue;
            }
            self.create_text_index_for(label, property)?;
            created.push((label.to_owned(), property.to_owned()));
        }
        Ok(created)
    }

    /// Create one BM25 text index. The single text-index create path, shared by the
    /// migration's [`Store::register_text_indexes`] and the recovery-time
    /// [`Store::ensure_catalog_indexes`], so neither can build one differently.
    fn create_text_index_for(&self, label: &str, property: &str) -> Result<(), StoreError> {
        let name = db_string(&format!("txt_{label}_{property}"))?;
        self.graph().create_text_index_named(
            db_string(label)?,
            db_string(property)?,
            Some(name),
        )?;
        Ok(())
    }

    /// Create every missing catalog scalar/property index — the per-`NODE_TYPES`
    /// `namespace` index (§11) and each [`SCALAR_INDEXES`] entry — returning the
    /// `(label, property)` pairs created (empty when all already existed).
    pub(crate) fn register_property_indexes(&self) -> Result<Vec<(String, String)>, StoreError> {
        let mut created = Vec::new();
        // namespace is indexed on every kind (§11). Skip a label the bound type does not
        // declare (see `register_vector_indexes`): only reachable on a partial recovery.
        for type_ddl in NODE_TYPES {
            if self.has_node_type(type_ddl.name)
                && self.ensure_property_index(type_ddl.name, "namespace", TypedIndexKind::String)?
            {
                created.push((type_ddl.name.to_owned(), "namespace".to_owned()));
            }
        }
        for &(label, property, kind) in SCALAR_INDEXES {
            if self.has_node_type(label) && self.ensure_property_index(label, property, kind)? {
                created.push((label.to_owned(), property.to_owned()));
            }
        }
        Ok(created)
    }

    /// Ensure one scalar property index exists. Returns `true` if it was created here,
    /// `false` if it already existed (so callers can report only the new ones).
    fn ensure_property_index(
        &self,
        label: &str,
        property: &str,
        kind: TypedIndexKind,
    ) -> Result<bool, StoreError> {
        if self.property_index_exists(label, property) {
            return Ok(false);
        }
        let name = db_string(&format!("pidx_{label}_{property}"))?;
        self.graph().create_property_index_named(
            db_string(label)?,
            db_string(property)?,
            kind,
            Some(name),
        )?;
        Ok(true)
    }

    /// Run the composite-index DDL (`CREATE INDEX IF NOT EXISTS`), which is idempotent by
    /// construction. Shared by the migration and the recovery-time
    /// [`Store::ensure_catalog_indexes`]. The DDL does not report which (if any) it
    /// created, so this returns nothing; granular per-composite reporting is left out
    /// (the engine treats an already-present composite as a no-op).
    pub(crate) fn register_composite_indexes(&self) -> Result<(), StoreError> {
        for &(label, ddl) in COMPOSITE_INDEXES {
            // Skip a label the bound type does not declare (see `register_vector_indexes`):
            // only reachable on a partial recovery, where the schema/latch checks own the
            // "schema is broken" verdict — composite DDL must not crash ahead of them.
            if !self.has_node_type(label) {
                continue;
            }
            self.execute(&BoundQuery::new(ddl))?;
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

    /// Per-index vector memory and health stats (selene 1.2 vector index stats).
    ///
    /// Read-only: takes one graph snapshot and reads each registered index's
    /// `memory_usage()` accounting. Surfaces the per-index footprint that makes the
    /// TurboQuant compression win measurable and the IVF rebuild recommendation that
    /// drives bounded maintenance.
    #[must_use]
    pub fn vector_index_stats(&self) -> Vec<VectorIndexStats> {
        let snapshot = self.graph().read();
        snapshot
            .iter_vector_index_entries()
            .map(|(label, property, kind, _dimension, _hnsw, _ivf, _name)| {
                let usage = snapshot
                    .vector_index_for(&label, &property)
                    .map(|index| index.memory_usage())
                    .unwrap_or_default();
                VectorIndexStats {
                    label: label.as_str().to_owned(),
                    property: property.as_str().to_owned(),
                    kind: format!("{kind:?}"),
                    indexed_rows: usage.indexed_rows,
                    estimated_index_bytes: usage.estimated_index_bytes,
                    estimated_reachable_bytes: usage.estimated_reachable_bytes,
                    is_turbo_quant: matches!(kind, VectorIndexKind::TurboQuantCosine),
                    ivf_rebuild_recommended: usage.ivf_rebuild_recommended(),
                }
            })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StoreConfig;
    use aionforge_domain::time::Timestamp;

    const DIM: u32 = 4;

    fn now() -> Timestamp {
        "2026-06-06T12:00:00-05:00[America/Chicago]"
            .parse()
            .expect("valid zoned datetime")
    }

    /// A migrated in-memory store at a small embedding dimension.
    fn migrated() -> Store {
        let store = Store::open_with_config(StoreConfig {
            embedding_dimension: DIM,
        })
        .expect("open store");
        store.migrate(&now()).expect("migrate");
        store
    }

    /// The live kind of `(label, property)`, Debug-rendered, or `None` if absent.
    fn live_kind(store: &Store, label: &str, property: &str) -> Option<String> {
        store
            .vector_indexes()
            .into_iter()
            .find(|index| index.label == label && index.property == property)
            .map(|index| index.kind)
    }

    /// Simulate catalog drift: replace a live index with one of a different kind /
    /// dimension, the way a store written before a catalog change would carry it.
    fn force_index(store: &Store, label: &str, property: &str, kind: VectorIndexKind, dim: u32) {
        store
            .graph()
            .drop_vector_index(db_string(label).unwrap(), db_string(property).unwrap())
            .expect("drop");
        store
            .create_vector_index_for(label, property, kind, dim)
            .expect("recreate at drifted kind");
    }

    #[test]
    fn reconcile_converges_a_drifted_kind_to_the_catalog() {
        let store = migrated();
        // Simulate a pre-TurboQuant store: Episode.embedding_v1 carries an HnswCosine index.
        force_index(
            &store,
            "Episode",
            "embedding_v1",
            VectorIndexKind::HnswCosine,
            DIM,
        );
        assert_eq!(
            live_kind(&store, "Episode", "embedding_v1").as_deref(),
            Some("HnswCosine"),
            "drift is in place before reconciliation"
        );

        let reconciled = store.reconcile_vector_index_kinds(DIM).expect("reconcile");

        // Exactly the drifted index is reported, with both kinds named for the metric line.
        assert_eq!(reconciled.len(), 1, "one index reconciled: {reconciled:?}");
        let row = &reconciled[0];
        assert_eq!(row.label, "Episode");
        assert_eq!(row.property, "embedding_v1");
        assert_eq!(row.from_kind, "HnswCosine");
        assert_eq!(row.to_kind, "TurboQuantCosine");

        // The live index now matches the catalog, at the same dimension, and the full set
        // is intact (drop+recreate did not lose any index).
        assert_eq!(
            live_kind(&store, "Episode", "embedding_v1").as_deref(),
            Some("TurboQuantCosine"),
            "Episode index converged to the catalog kind"
        );
        assert_eq!(store.vector_indexes().len(), 7, "no index was lost");
        assert!(
            store
                .vector_indexes()
                .iter()
                .all(|index| index.dimension == DIM),
            "dimension preserved across reconciliation"
        );

        // The doctor — which detects kind mismatches independently — now reports clean.
        let report = store.doctor_report().expect("doctor report");
        assert!(
            report.indexes.vector_kind_mismatches.is_empty(),
            "doctor sees no kind mismatch after reconciliation: {:?}",
            report.indexes.vector_kind_mismatches
        );
        assert!(report.indexes.ok, "index health is restored");
    }

    #[test]
    fn reconcile_is_idempotent() {
        let store = migrated();
        force_index(
            &store,
            "Fact",
            "embedding_v1",
            VectorIndexKind::HnswCosine,
            DIM,
        );

        let first = store.reconcile_vector_index_kinds(DIM).expect("first");
        assert_eq!(first.len(), 1, "first run converges the drift");

        // A clean store reconciles nothing — the all-catalog state is a fixed point.
        let second = store.reconcile_vector_index_kinds(DIM).expect("second");
        assert!(second.is_empty(), "second run is a no-op: {second:?}");
    }

    #[test]
    fn reconcile_skips_a_dimension_mismatch_for_the_dimension_check() {
        let store = migrated();
        // A drifted kind AND a wrong dimension: the embedder-changed (lossy) case.
        // Reconciliation is dimension-preserving, so it must leave this index untouched
        // and let `dimension_consistency_check` reject it loudly instead.
        force_index(
            &store,
            "Episode",
            "embedding_v1",
            VectorIndexKind::HnswCosine,
            DIM * 2,
        );

        let reconciled = store.reconcile_vector_index_kinds(DIM).expect("reconcile");
        assert!(
            reconciled.is_empty(),
            "dimension-mismatched index is left for the dimension check: {reconciled:?}"
        );
        assert_eq!(
            live_kind(&store, "Episode", "embedding_v1").as_deref(),
            Some("HnswCosine"),
            "the mismatched index is untouched, not silently rebuilt"
        );
        assert!(
            store.dimension_consistency_check(DIM).is_err(),
            "the dimension check still fails loudly on the real embedder change"
        );
    }
}
