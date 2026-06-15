//! The forward-only, idempotent schema migration runner (data-model §12).
//!
//! The runner applies the [`crate::catalog`] DDL to a closed graph and tracks how far
//! it has gone in the `SchemaVersion` singleton. Idempotency is belt-and-suspenders:
//! the runner skips when the recorded version already meets the target, and every DDL
//! statement carries `IF NOT EXISTS`, so even a forced re-apply is a no-op at the
//! engine level. There is no down path — a migration only ever moves the version
//! forward.

use std::collections::HashSet;

use aionforge_domain::time::Timestamp;
use aionforge_domain::{Id, Namespace};
use selene_core::Value;

use crate::catalog::{EDGE_TYPES, NODE_TYPES, SCHEMA_VERSION, TypeDdl};
use crate::convert::timestamp_value;
use crate::error::StoreError;
use crate::gql::{BoundQuery, QueryResult};
use crate::store::Store;

/// What a [`Store::migrate`] run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    /// The schema version before the run.
    pub from_version: i64,
    /// The schema version after the run.
    pub to_version: i64,
    /// The names of the types this run newly created, in declaration order.
    ///
    /// Empty when the schema was already current (the no-op case) or when every type
    /// already existed.
    pub applied: Vec<String>,
}

impl MigrationReport {
    /// True when the run changed nothing — the schema was already at the target.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.from_version == self.to_version && self.applied.is_empty()
    }
}

/// A type a [`Store::migration_plan`] dry-run would create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingChange {
    /// The node label / edge relationship name that would be created.
    pub name: String,
    /// The exact statement the migration would run.
    pub ddl: String,
}

/// What a migration would do, computed without writing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationPlan {
    /// The schema version recorded now.
    pub current_version: i64,
    /// The version a migration would bring the schema to.
    pub target_version: i64,
    /// The types that do not yet exist and would be created, in declaration order.
    pub pending: Vec<PendingChange>,
}

impl MigrationPlan {
    /// True when nothing is pending — a migration would be a no-op.
    #[must_use]
    pub fn is_current(&self) -> bool {
        self.pending.is_empty()
    }
}

/// What [`Store::reconcile_additive_schema`] did on a recovered binding. Internal
/// observability for the recovery path (surfaced via metrics/logs by the caller), not part
/// of the public store API — `recover` returns the opened store, not this report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdditiveSchemaReconciliation {
    /// The catalog types this run newly created, in declaration order (node types before
    /// edge types). Empty when the recovered binding already declared every catalog type.
    pub(crate) created: Vec<String>,
    /// The version the recorded `SchemaVersion` was advanced to, or `None` when it was
    /// already current (the common case) — or the binding had not fully converged to the
    /// catalog, so the version is left untouched.
    pub(crate) version_advanced_to: Option<i64>,
}

/// Every catalog entry, node types first so edge endpoints can resolve their labels.
fn catalog() -> impl Iterator<Item = &'static TypeDdl> {
    NODE_TYPES.iter().chain(EDGE_TYPES.iter())
}

impl Store {
    /// Apply the schema forward to [`SCHEMA_VERSION`], idempotently.
    ///
    /// Re-running after a completed migration is a no-op (the version guard returns
    /// early). `now` stamps the `SchemaVersion` singleton's `applied_at`.
    ///
    /// # Errors
    /// Returns [`StoreError`] if reading the version or applying any statement fails.
    pub fn migrate(&self, now: &Timestamp) -> Result<MigrationReport, StoreError> {
        let from_version = self.schema_version()?;
        if from_version >= SCHEMA_VERSION {
            // Already current — but assert the binding still admits the audit-signature
            // latch (02 §4.11) before declaring the no-op: a pre-latch binding cannot be
            // migrated forward (no `ALTER TYPE`), so it must fail loudly here, not at
            // some later commit's in-place signature upgrade. `Store::recover` runs the
            // same check for stores that never pass through `migrate`.
            self.audit_signature_latch_check()?;
            tracing::debug!(
                target: "aionforge::store",
                from_version,
                "schema already current; migration is a no-op",
            );
            return Ok(MigrationReport {
                from_version,
                to_version: from_version,
                applied: Vec::new(),
            });
        }

        let existing_before = self.existing_type_names()?;
        for type_ddl in catalog() {
            self.execute(&BoundQuery::new(type_ddl.ddl))?;
        }
        let applied: Vec<String> = catalog()
            .filter(|type_ddl| !existing_before.contains(type_ddl.name))
            .map(|type_ddl| type_ddl.name.to_owned())
            .collect();

        // Indexes (§7–§8) are part of the schema the migration applies; providers (§9)
        // are attached at construction, not here. Run before the version is recorded so
        // a crash mid-migration re-runs idempotently.
        self.register_indexes(self.config().embedding_dimension)?;

        // §13.5: every vector index's dimension must equal the configured embedder
        // dimension. Assert it here, after the indexes exist and before the version is
        // recorded, so a mismatch is a loud migration failure and a crash re-validates on
        // re-run. The recovery path (a recovered graph skips this guard via the version
        // check above) re-runs the same check at open time.
        self.dimension_consistency_check(self.config().embedding_dimension)?;

        self.write_schema_version(SCHEMA_VERSION, now)?;
        // `migrate` was otherwise silent (no metrics, no logs) despite running DDL, index
        // registration, the dimension check, and the version write — make the version step
        // visible (logging hot-paths, task #9 PR2). Low-cardinality integers only.
        tracing::info!(
            target: "aionforge::store",
            from_version,
            to_version = SCHEMA_VERSION,
            applied = applied.len(),
            "schema migrated",
        );
        Ok(MigrationReport {
            from_version,
            to_version: SCHEMA_VERSION,
            applied,
        })
    }

    /// Compute what a migration would create, without writing anything (dry run).
    ///
    /// # Errors
    /// Returns [`StoreError`] if reading the current version or schema fails.
    pub fn migration_plan(&self) -> Result<MigrationPlan, StoreError> {
        let current_version = self.schema_version()?;
        let existing = self.existing_type_names()?;
        let pending = catalog()
            .filter(|type_ddl| !existing.contains(type_ddl.name))
            .map(|type_ddl| PendingChange {
                name: type_ddl.name.to_owned(),
                ddl: type_ddl.ddl.to_owned(),
            })
            .collect();
        Ok(MigrationPlan {
            current_version,
            target_version: SCHEMA_VERSION,
            pending,
        })
    }

    /// Create any catalog node/edge TYPE the recovered binding is MISSING, idempotently,
    /// returning the type names created (empty when the binding already declared them all).
    ///
    /// [`Store::recover`] replays a persisted schema, but the version-guarded
    /// [`Store::migrate`] only declares types on a FRESH migration — a store first migrated
    /// under an older catalog never gains a type added to the catalog later. This closes
    /// that gap with the one schema change that is non-lossy in place: an additive
    /// `CREATE ... TYPE IF NOT EXISTS`, which never touches existing rows. It is the
    /// type-layer companion to [`Store::ensure_catalog_indexes`](crate::Store::ensure_catalog_indexes)
    /// (missing indexes) and
    /// [`Store::reconcile_vector_index_kinds`](crate::Store::reconcile_vector_index_kinds)
    /// (drifted index kinds), and the recovery path runs it BEFORE both so a newly created
    /// type's indexes are then built by `ensure_catalog_indexes` (whose per-class registrars
    /// skip a type the binding does not declare). DRY: it reuses the same `catalog()` DDL
    /// the migration applies, so the fresh and recovered paths cannot drift.
    ///
    /// Recording the recorded `SchemaVersion` is deliberately NOT done here — see
    /// [`Store::advance_recorded_schema_version_if_converged`], which the recovery path runs
    /// LAST, after the dimension and audit-signature-latch checks, so a version is never
    /// persisted on a binding that then fails to validate (mirroring `migrate`, which records
    /// the version only after its own checks pass).
    ///
    /// Idempotent: a second run finds nothing missing and returns an empty `Vec`.
    ///
    /// # Errors
    /// Returns [`StoreError`] if creating a type fails.
    pub(crate) fn reconcile_additive_schema(&self) -> Result<Vec<String>, StoreError> {
        let existing_before = self.existing_type_names()?;
        let mut created = Vec::new();
        for type_ddl in catalog() {
            if existing_before.contains(type_ddl.name) {
                continue;
            }
            self.execute(&BoundQuery::new(type_ddl.ddl))?;
            created.push(type_ddl.name.to_owned());
        }
        Ok(created)
    }

    /// Advance the recorded `SchemaVersion` to [`SCHEMA_VERSION`] when, and only when, the
    /// live binding already declares every catalog type and the recorded version is behind.
    /// Returns the version stamped, or `None` when nothing was recorded.
    ///
    /// The recovery path calls this LAST — after [`Store::reconcile_additive_schema`] has
    /// created any missing type AND after the dimension-consistency and audit-signature-latch
    /// checks have validated the recovered binding — so the persisted version can never claim
    /// a convergence the binding does not satisfy. This matches the sibling [`Store::migrate`],
    /// which records the version only after its index registration and dimension check pass.
    ///
    /// # Soundness
    /// The catalog is additive-only — every statement is `CREATE ... IF NOT EXISTS`; the
    /// migration path has no `ALTER`/`DROP` — so once every catalog type is declared the
    /// binding's type shape equals [`SCHEMA_VERSION`]'s, and recording that version is honest.
    /// A breaking change (a type's columns, or the audit-signature latch) is NOT expressible
    /// additively and stays gated to 1.0.0; the latch is separately enforced by
    /// [`Store::audit_signature_latch_check`](crate::Store::audit_signature_latch_check), which
    /// the caller runs BEFORE this. When a non-additive migration lands, this gate (which
    /// checks only that every type NAME is present, not its shape) must become
    /// version-delta-aware. The presence check is recomputed from the live binding, and the
    /// version is only ever advanced forward — never downgraded below a binding already ahead
    /// of this build's catalog. `now` stamps `SchemaVersion.applied_at`.
    ///
    /// # Errors
    /// Returns [`StoreError`] if reading the version or recording it fails.
    pub(crate) fn advance_recorded_schema_version_if_converged(
        &self,
        now: &Timestamp,
    ) -> Result<Option<i64>, StoreError> {
        let recorded = self.schema_version()?;
        if recorded < SCHEMA_VERSION && self.declares_every_catalog_type()? {
            self.write_schema_version(SCHEMA_VERSION, now)?;
            Ok(Some(SCHEMA_VERSION))
        } else {
            Ok(None)
        }
    }

    /// Whether the bound graph type declares every catalog node and edge type.
    fn declares_every_catalog_type(&self) -> Result<bool, StoreError> {
        let existing = self.existing_type_names()?;
        Ok(catalog().all(|type_ddl| existing.contains(type_ddl.name)))
    }

    /// The applied schema version, or `0` when no `SchemaVersion` singleton exists yet.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the read query fails or the stored value is not an
    /// integer.
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        if !self.has_node_type("SchemaVersion") {
            return Ok(0);
        }
        let query = BoundQuery::new("MATCH (v:SchemaVersion) RETURN v.current_version AS version");
        match self.execute(&query)? {
            QueryResult::Rows(rows) => match rows.value(0, 0) {
                Some(Value::Int(version)) => Ok(*version),
                None => Ok(0),
                Some(other) => Err(StoreError::decode(format!(
                    "SchemaVersion.current_version was not an integer: {other:?}"
                ))),
            },
            // A `MATCH ... RETURN` always yields rows; anything else is an engine
            // anomaly, not "no version" — surface it rather than silently reading 0.
            other => Err(StoreError::decode(format!(
                "SchemaVersion read returned a non-row result: {other:?}"
            ))),
        }
    }

    /// The set of node-type names and edge relationship names currently declared.
    fn existing_type_names(&self) -> Result<HashSet<String>, StoreError> {
        let mut names = HashSet::new();
        if let Some(graph_type) = self.graph().graph_type() {
            for node_type in &graph_type.node_types {
                names.insert(node_type.name.as_str().to_owned());
            }
            for edge_type in &graph_type.edge_types {
                names.insert(edge_type.label.as_str().to_owned());
            }
        }
        Ok(names)
    }

    /// Whether a node type with this label is declared in the bound graph type.
    pub(crate) fn has_node_type(&self, name: &str) -> bool {
        self.graph()
            .graph_type()
            .as_deref()
            .is_some_and(|graph_type| {
                graph_type
                    .node_types
                    .iter()
                    .any(|node_type| node_type.name.as_str() == name)
            })
    }

    /// Upsert the singleton `SchemaVersion` node to `version`.
    ///
    /// Enforces the singleton: an instance is updated in place rather than a second
    /// one created. In the v0→v1 path there is no instance yet, so this creates it.
    fn write_schema_version(&self, version: i64, now: &Timestamp) -> Result<(), StoreError> {
        if self.schema_version_instance_exists()? {
            let query = BoundQuery::new(
                "MATCH (v:SchemaVersion) SET v.current_version = $version, v.applied_at = $now",
            )
            .bind("version", Value::Int(version))?
            .bind("now", timestamp_value(now))?;
            self.execute(&query)?;
        } else {
            let query = BoundQuery::new(
                "INSERT (v:SchemaVersion {id: $id, ingested_at: $now, namespace: $namespace, \
                 current_version: $version, applied_at: $now})",
            )
            .bind_uuid("id", Id::generate())?
            .bind("now", timestamp_value(now))?
            .bind_str("namespace", &Namespace::System.to_string())?
            .bind("version", Value::Int(version))?;
            self.execute(&query)?;
        }
        Ok(())
    }

    /// Whether a `SchemaVersion` node instance already exists.
    fn schema_version_instance_exists(&self) -> Result<bool, StoreError> {
        let query = BoundQuery::new("MATCH (v:SchemaVersion) RETURN v.id AS id");
        match self.execute(&query)? {
            QueryResult::Rows(rows) => Ok(rows.row_count() > 0),
            // A `MATCH ... RETURN` always yields rows; treat anything else as an error
            // rather than assuming no instance exists (which would risk a duplicate).
            other => Err(StoreError::decode(format!(
                "SchemaVersion existence check returned a non-row result: {other:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StoreConfig;

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

    /// Tear the v3 work-tracking `Tag`/`HAS_TAG` facet off a migrated binding, the way a
    /// store first migrated under an older catalog simply never declared it. The edge is
    /// dropped first: `RESTRICT` (selene's default) refuses to drop a node type with an
    /// inbound edge-type reference, and `CASCADE` on the node then clears its (zero)
    /// instances and indexes.
    fn drop_tag_facet(store: &Store) {
        store
            .execute(&BoundQuery::new("DROP EDGE TYPE IF EXISTS :HAS_TAG"))
            .expect("drop HAS_TAG edge type");
        store
            .execute(&BoundQuery::new("DROP NODE TYPE IF EXISTS :Tag CASCADE"))
            .expect("drop Tag node type");
    }

    /// Wind the recorded `SchemaVersion` back to `version`, simulating a store first
    /// migrated under an older catalog.
    fn set_recorded_version(store: &Store, version: i64) {
        store
            .execute(
                &BoundQuery::new("MATCH (v:SchemaVersion) SET v.current_version = $version")
                    .bind("version", Value::Int(version))
                    .expect("bind version"),
            )
            .expect("set recorded version");
    }

    /// Whether the bound graph type declares `name` (node or edge).
    fn declares(store: &Store, name: &str) -> bool {
        store
            .existing_type_names()
            .expect("type names")
            .contains(name)
    }

    #[test]
    fn reconcile_additive_schema_recreates_missing_types_node_first() {
        let store = migrated();
        drop_tag_facet(&store);
        assert!(!declares(&store, "Tag"), "Tag is gone before reconcile");
        assert!(
            !declares(&store, "HAS_TAG"),
            "HAS_TAG is gone before reconcile"
        );

        let created = store.reconcile_additive_schema().expect("reconcile");

        assert_eq!(
            created,
            vec!["Tag".to_owned(), "HAS_TAG".to_owned()],
            "exactly the missing node + edge are recreated, node before edge: {created:?}"
        );
        assert!(declares(&store, "Tag"), "Tag is recreated");
        assert!(declares(&store, "HAS_TAG"), "HAS_TAG is recreated");
    }

    #[test]
    fn reconcile_additive_schema_is_a_noop_on_a_current_store() {
        let store = migrated();
        let created = store.reconcile_additive_schema().expect("reconcile");
        assert!(
            created.is_empty(),
            "a freshly migrated store declares every type already: {created:?}"
        );
    }

    #[test]
    fn reconcile_additive_schema_is_idempotent() {
        let store = migrated();
        drop_tag_facet(&store);

        let first = store.reconcile_additive_schema().expect("first");
        assert!(
            !first.is_empty(),
            "the first run recreates the facet: {first:?}"
        );

        let second = store.reconcile_additive_schema().expect("second");
        assert!(
            second.is_empty(),
            "the second run finds nothing missing: {second:?}"
        );
    }

    #[test]
    fn reconcile_additive_schema_does_not_advance_the_recorded_version() {
        // Type creation must NEVER touch the recorded version: the recovery path stamps it
        // separately and LAST, after validation. Even on a version-behind store that this
        // run brings whole, `reconcile_additive_schema` alone leaves the version untouched.
        let store = migrated();
        drop_tag_facet(&store);
        set_recorded_version(&store, SCHEMA_VERSION - 1);

        let created = store.reconcile_additive_schema().expect("reconcile");

        assert!(!created.is_empty(), "the facet is recreated: {created:?}");
        assert_eq!(
            store.schema_version().expect("version"),
            SCHEMA_VERSION - 1,
            "creating types does not stamp the version — that is the stamp step's job"
        );
    }

    #[test]
    fn advance_version_stamps_when_behind_and_whole() {
        let store = migrated();
        // A fully-declared store whose recorded version is one behind — the converged case.
        set_recorded_version(&store, SCHEMA_VERSION - 1);

        let advanced = store
            .advance_recorded_schema_version_if_converged(&now())
            .expect("advance");

        assert_eq!(
            advanced,
            Some(SCHEMA_VERSION),
            "the version advances once the binding declares the full catalog"
        );
        assert_eq!(
            store.schema_version().expect("version"),
            SCHEMA_VERSION,
            "the advance is persisted"
        );
    }

    #[test]
    fn advance_version_is_a_noop_when_already_current() {
        let store = migrated();
        let advanced = store
            .advance_recorded_schema_version_if_converged(&now())
            .expect("advance");
        assert_eq!(
            advanced, None,
            "an already-current version is left untouched"
        );
        assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
    }

    #[test]
    fn advance_version_refuses_when_a_type_is_missing() {
        // The gate the version honesty depends on: a binding that does NOT declare every
        // catalog type must not get its version stamped, even when the recorded version is
        // behind. This is what protects against stamping a not-yet-converged store.
        let store = migrated();
        drop_tag_facet(&store);
        set_recorded_version(&store, SCHEMA_VERSION - 1);

        let advanced = store
            .advance_recorded_schema_version_if_converged(&now())
            .expect("advance");

        assert_eq!(
            advanced, None,
            "a binding missing a catalog type is not stamped"
        );
        assert_eq!(
            store.schema_version().expect("version"),
            SCHEMA_VERSION - 1,
            "the recorded version stays behind until the binding is whole"
        );
    }
}
