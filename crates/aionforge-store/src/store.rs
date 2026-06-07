//! The L0 store: a single owned `SharedGraph` with typed read/write and
//! parameter-bound GQL.

use std::path::Path;
use std::sync::Arc;

use aionforge_domain::nodes::episodic::Episode;
use aionforge_domain::time::Timestamp;
use selene_core::{GraphId, NodeId, db_string};
use selene_gql::{BindingTable, BuiltinProcedureRegistry, Session, StatementOutput};
use selene_graph::{DEFAULT_WAL_FILE_NAME, GraphTypeDef, SeleneGraph, SharedGraph, WalConfig};

use crate::config::StoreConfig;
use crate::episode;
use crate::error::StoreError;
use crate::gql::{BoundQuery, QueryResult, Rows};
use crate::providers::candidate_state_provider;

/// The storage layer over a selene-db `SharedGraph`.
///
/// Owns one graph for the process lifetime (constructing it spawns the engine's
/// single committer thread, which is the sole snapshot publisher). Every write —
/// the typed [`Store::insert_episode`] path and any data-modifying statement the
/// engine auto-commits inside [`Store::execute`] — commits through that one
/// committer, durable before visible. Reads take a lock-free snapshot. Every
/// caller-influenced value travels as a bound parameter, never spliced into the
/// query text.
///
/// The graph is opened *closed* (bound to a graph type), because selene-db rejects
/// catalog DDL on an open graph. A freshly opened store carries an empty type and
/// holds no kinds until [`Store::migrate`] declares them; inserting a typed node
/// before its kind is declared fails fast against the closed-graph validator.
pub struct Store {
    graph: SharedGraph,
    config: StoreConfig,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // SharedGraph is not Debug, so we do not recurse into it.
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// Open an in-memory store with no persistence and no schema applied yet.
    ///
    /// The graph is closed but bound to an empty type, so it accepts catalog DDL but
    /// holds no kinds — call [`Store::migrate`] to declare the schema. This store keeps
    /// everything in memory and writes nothing to disk; for WAL-backed durability use
    /// [`Store::open_persistent`], [`Store::recover`], or [`Store::open_or_recover`].
    ///
    /// # Errors
    /// Returns [`StoreError`] if the empty graph type fails the engine's
    /// self-consistency check (it does not for an empty type, but the binding path is
    /// fallible).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::open_with_config(StoreConfig::default())
    }

    /// Open an in-memory store with explicit configuration (no schema applied yet).
    ///
    /// The candidate-state providers (data-model §9) are attached here at construction,
    /// because they are not migration objects — a provider that is not attached at build
    /// time does not exist for the process.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the empty graph type or the provider registration fails.
    pub fn open_with_config(config: StoreConfig) -> Result<Self, StoreError> {
        let graph = SharedGraph::builder(graph_id())
            .bound_to(empty_graph_type()?)?
            .with_provider(candidate_state_provider()?)
            .build()?;
        Ok(Self { graph, config })
    }

    /// Open a WAL-backed store at `dir`, with no schema applied yet.
    ///
    /// The graph is the same closed, provider-bound shape as
    /// [`Store::open_with_config`], but every commit is now appended to a durable
    /// write-ahead log at `dir/<wal>` before it becomes visible. `dir` is created if
    /// it does not exist. Call [`Store::migrate`] to declare the schema; the DDL and
    /// index registration are persisted to the WAL and replayed on
    /// [`Store::recover`].
    ///
    /// # Errors
    /// Returns [`StoreError`] if `dir` cannot be created, the WAL cannot be opened
    /// (including when another writer already holds its lock), or the graph type or
    /// provider registration fails.
    pub fn open_persistent(dir: &Path, config: StoreConfig) -> Result<Self, StoreError> {
        std::fs::create_dir_all(dir).map_err(|err| {
            StoreError::persist(format!(
                "cannot create the store directory {}: {err}",
                dir.display()
            ))
        })?;
        let graph = SharedGraph::builder(graph_id())
            .with_wal(dir.join(DEFAULT_WAL_FILE_NAME), WalConfig::default())?
            .bound_to(empty_graph_type()?)?
            .with_provider(candidate_state_provider()?)
            .build()?;
        Ok(Self { graph, config })
    }

    /// Open a WAL-backed store at `dir` with the full schema already applied.
    ///
    /// Equivalent to [`Store::open_persistent`] followed by [`Store::migrate`]. `now`
    /// stamps the `SchemaVersion` singleton.
    ///
    /// # Errors
    /// Returns [`StoreError`] if opening or migrating fails.
    pub fn open_persistent_migrated(
        dir: &Path,
        config: StoreConfig,
        now: &Timestamp,
    ) -> Result<Self, StoreError> {
        let store = Self::open_persistent(dir, config)?;
        store.migrate(now)?;
        Ok(store)
    }

    /// Recover a WAL-backed store from `dir`.
    ///
    /// The closed binding is reconstructed by replaying the persisted schema DDL onto
    /// an empty type, the indexes are rebuilt from primary values, and the
    /// candidate-state providers (data-model §9) are re-attached so post-recovery
    /// commits maintain the same sets. The empty baseline is correct for the WAL-only
    /// shape this store writes (no on-disk snapshots yet); once snapshots/compaction
    /// land, the recovery baseline must come from the snapshot's recorded type.
    ///
    /// Recovery does not migrate — the schema is already present in the replayed log —
    /// but it does re-run the §13.5 dimension-consistency check, which the version-
    /// guarded [`Store::migrate`] would skip on an already-current graph.
    ///
    /// # Errors
    /// Returns [`StoreError`] if recovery fails (corrupt or mismatched persistence,
    /// type drift, or a recovered vector index whose dimension disagrees with
    /// `config`).
    pub fn recover(dir: &Path, config: StoreConfig) -> Result<Self, StoreError> {
        let graph = SharedGraph::recover_closed_with_providers(
            dir,
            graph_id(),
            empty_graph_type()?,
            vec![candidate_state_provider()?],
        )?;
        let store = Self { graph, config };
        store.dimension_consistency_check(config.embedding_dimension)?;
        Ok(store)
    }

    /// Open the store at `dir`, recovering existing persistence or creating it fresh.
    ///
    /// If a WAL is present at `dir`, recover from it; otherwise create the directory,
    /// open fresh, and migrate. This is the ready-to-use entry point for a durable
    /// store whose first run and later runs take the same call. `now` stamps the
    /// `SchemaVersion` on the first run and is unused on recovery.
    ///
    /// # Errors
    /// Returns [`StoreError`] if recovery or fresh open/migration fails.
    pub fn open_or_recover(
        dir: &Path,
        config: StoreConfig,
        now: &Timestamp,
    ) -> Result<Self, StoreError> {
        if dir.join(DEFAULT_WAL_FILE_NAME).exists() {
            Self::recover(dir, config)
        } else {
            Self::open_persistent_migrated(dir, config, now)
        }
    }

    /// Open an in-memory store with the full schema already applied.
    ///
    /// Equivalent to [`Store::open_in_memory`] followed by [`Store::migrate`]; this is
    /// the ready-to-use shape callers want when they are not exercising the migration
    /// machinery itself. `now` stamps the `SchemaVersion` singleton.
    ///
    /// # Errors
    /// Returns [`StoreError`] if opening or migrating fails.
    pub fn open_in_memory_migrated(now: &Timestamp) -> Result<Self, StoreError> {
        let store = Self::open_in_memory()?;
        store.migrate(now)?;
        Ok(store)
    }

    /// The owned shared graph, for the schema and migration machinery in this crate.
    pub(crate) fn graph(&self) -> &SharedGraph {
        &self.graph
    }

    /// This store's binding configuration.
    #[must_use]
    pub fn config(&self) -> StoreConfig {
        self.config
    }

    /// Take a lock-free read snapshot of the current graph state.
    ///
    /// The returned `Arc` pins that snapshot version; drop it promptly (per
    /// statement) so superseded snapshots are reclaimed.
    #[must_use]
    pub fn snapshot(&self) -> Arc<SeleneGraph> {
        self.graph.read()
    }

    /// Commit an episode through the single write funnel, returning its node id.
    ///
    /// # Errors
    /// Returns [`StoreError`] if translation, the mutation, or the commit fails.
    pub fn insert_episode(&self, episode: &Episode) -> Result<NodeId, StoreError> {
        let (labels, props) = episode::to_node(episode)?;
        let mut txn = self.graph.begin_write();
        let node_id = {
            let mut mutator = txn.mutator();
            mutator.create_node(labels, props)?
        };
        txn.commit()?;
        Ok(node_id)
    }

    /// Read an episode back by its node id from a fresh snapshot.
    ///
    /// Returns `Ok(None)` if no live node with that id exists.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the stored data cannot be decoded into an
    /// [`Episode`].
    pub fn episode_by_node_id(&self, id: NodeId) -> Result<Option<Episode>, StoreError> {
        let snapshot = self.graph.read();
        match snapshot.node_properties(id) {
            Some(props) => Ok(Some(episode::from_properties(props)?)),
            None => Ok(None),
        }
    }

    /// Execute a parameter-bound GQL statement.
    ///
    /// The query's source is fixed and trusted; every caller value travels as a
    /// bound parameter, so the parsed statement never depends on caller input.
    /// Statements run against the engine's full builtin procedure registry, so the
    /// native `CALL selene.*` / `CALL algo.*` surfaces (vector, BM25, candidate-state,
    /// and graph algorithms — 03 §1–§4) are available through this one seam.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the statement fails to parse, plan, or execute.
    pub fn execute(&self, query: &BoundQuery) -> Result<QueryResult, StoreError> {
        let mut session = Session::new(&self.graph);
        for (name, value) in query.params() {
            session.bind_parameter(name.clone(), value.clone());
        }
        let registry = BuiltinProcedureRegistry::new();
        let output = session.execute_source(query.source(), &registry)?;
        materialize(output)
    }
}

/// This store's fixed graph identity. A single graph per store, so a constant id;
/// recovery asserts the same value against the persisted metadata.
fn graph_id() -> GraphId {
    GraphId::new(1)
}

/// The closed binding every store opens with: bound (so catalog DDL is accepted) but
/// empty, so it holds no kinds until [`Store::migrate`] declares them. Recovery replays
/// the persisted DDL onto this same empty baseline, so the value must match the one used
/// at fresh open.
fn empty_graph_type() -> Result<GraphTypeDef, StoreError> {
    Ok(GraphTypeDef {
        name: db_string("aionforge.memory")?,
        node_types: Vec::new(),
        edge_types: Vec::new(),
    })
}

/// Convert an owned engine [`StatementOutput`] into the owned [`QueryResult`].
///
/// A data-modifying statement carrying a `RETURN` auto-commits and still yields
/// rows; those are carried through on [`QueryResult::Written`] rather than dropped.
fn materialize(output: StatementOutput) -> Result<QueryResult, StoreError> {
    match output {
        StatementOutput::Empty => Ok(QueryResult::Empty),
        StatementOutput::Written(outcome) => Ok(QueryResult::Written {
            generation: outcome.generation,
            rows: outcome.rows.map(materialize_table),
        }),
        StatementOutput::Rows(table) => Ok(QueryResult::Rows(materialize_table(table))),
        other => Err(StoreError::decode(format!(
            "unrecognized statement output: {other:?}"
        ))),
    }
}

/// Materialize an owned engine binding table into the owned [`Rows`].
fn materialize_table(table: BindingTable) -> Rows {
    let columns = table
        .schema()
        .columns
        .iter()
        .map(|column| column.name.as_ref().map(|name| name.as_str().to_string()))
        .collect();
    let rows = table
        .iter()
        .map(|binding| binding.values().to_vec())
        .collect();
    Rows::new(columns, rows)
}
