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
