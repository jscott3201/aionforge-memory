//! The L0 store: a single owned `SharedGraph` with typed read/write and
//! parameter-bound GQL.

use std::path::Path;
use std::sync::Arc;

use aionforge_domain::edges::{About, Audit, Contradicts, HasProvenance, SupersededBy};
use aionforge_domain::embedding::Embedding;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::episodic::Episode;
use aionforge_domain::nodes::forensic::{AuditEvent, ProvenanceRecord};
use aionforge_domain::nodes::semantic::{Entity, Fact};
use aionforge_domain::time::Timestamp;
use selene_core::{GraphId, NodeId, PropertyMap, db_string};
use selene_gql::{BindingTable, BuiltinProcedureRegistry, Session, StatementOutput};
use selene_graph::{DEFAULT_WAL_FILE_NAME, GraphTypeDef, SeleneGraph, SharedGraph, WalConfig};

use crate::config::StoreConfig;
use crate::convert::as_id;
use crate::error::StoreError;
use crate::gql::{BoundQuery, QueryResult, Rows};
use crate::providers::candidate_state_provider;
use crate::search::SearchKind;
use crate::{audit, entity, episode, fact, provenance};

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

    /// Run a sequence of parameter-bound statements in one engine session, returning the
    /// last statement's result.
    ///
    /// Unlike [`Store::execute`] — which opens a fresh session per call — every statement
    /// here shares one session. selene graph-algorithm projections live in the session,
    /// not the graph, so a projection a `CALL algo.projection_build` statement registers is
    /// visible to a later `CALL algo.pagerank` over it; running them through two separate
    /// `execute` calls would lose the projection between them. Each source is fixed and
    /// trusted and every caller value travels as a bound parameter, exactly as in
    /// [`Store::execute`].
    ///
    /// # Errors
    /// Returns [`StoreError`] if any statement fails to parse, plan, or execute.
    pub(crate) fn execute_session(
        &self,
        statements: &[BoundQuery],
    ) -> Result<QueryResult, StoreError> {
        let registry = BuiltinProcedureRegistry::new();
        let mut session = Session::new(&self.graph);
        let mut result = QueryResult::Empty;
        for query in statements {
            for (name, value) in query.params() {
                session.bind_parameter(name.clone(), value.clone());
            }
            result = materialize(session.execute_source(query.source(), &registry)?)?;
        }
        Ok(result)
    }

    /// The id of a live episode with this content hash, if one exists.
    ///
    /// The exact-duplicate check on the capture path (04 §1): `content_hash` is an
    /// indexed `STRING` column, so this is an index probe, not a scan. Only active
    /// episodes match — a soft-forgotten one (`expired_at` set, 02 §3) must not block
    /// re-capturing the same content, since soft-forget is reversible (05 §2).
    ///
    /// # Errors
    /// Returns [`StoreError`] if the query fails or the stored id is malformed.
    pub fn episode_id_by_content_hash(
        &self,
        content_hash: &ContentHash,
    ) -> Result<Option<Id>, StoreError> {
        let query = BoundQuery::new(
            "MATCH (e:Episode) WHERE e.content_hash = $h AND e.expired_at IS NULL \
             RETURN e.id AS id LIMIT 1",
        )
        .bind_str("h", content_hash.as_str())?;
        match self.execute(&query)? {
            QueryResult::Rows(rows) => match rows.value(0, 0) {
                Some(value) => Ok(Some(as_id(value)?)),
                None => Ok(None),
            },
            _ => Ok(None),
        }
    }

    /// The nearest *active* episode to `query` and its cosine distance, if any.
    ///
    /// The near-duplicate check on the capture path (04 §1 step 2). It scans the top
    /// `k` ANN neighbors best-first and returns the first one that is still active
    /// (`expired_at` unset, 02 §3), skipping soft-forgotten episodes so a near-dup is
    /// never judged against a forgotten memory. Returning the domain [`Id`] keeps the
    /// engine's `NodeId` inside this layer. The caller applies its own similarity
    /// threshold to the returned distance (smaller is more similar).
    ///
    /// # Errors
    /// Returns [`StoreError`] if the search, a candidate read, or a decode fails.
    pub fn nearest_active_episode(
        &self,
        query: &Embedding,
        k: usize,
    ) -> Result<Option<(Id, f64)>, StoreError> {
        for hit in self.vector_search_ann(SearchKind::Episode, query, k)? {
            if let Some(episode) = self.episode_by_node_id(hit.node)?
                && episode.identity.expired_at.is_none()
            {
                return Ok(Some((episode.identity.id, hit.score)));
            }
        }
        Ok(None)
    }

    // --- Semantic tier: facts and entities (02 §4.2, §4.3; M2.T01) -----------------

    /// Commit an entity node through the single write funnel, returning its node id.
    ///
    /// # Errors
    /// Returns [`StoreError`] if translation, the mutation, or the commit fails.
    pub fn insert_entity(&self, entity: &Entity) -> Result<NodeId, StoreError> {
        let (labels, props) = entity::to_node(entity)?;
        let mut txn = self.graph.begin_write();
        let node_id = {
            let mut mutator = txn.mutator();
            mutator.create_node(labels, props)?
        };
        txn.commit()?;
        Ok(node_id)
    }

    /// Read an entity back by its node id from a fresh snapshot.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the stored data cannot be decoded into an [`Entity`].
    pub fn entity_by_node_id(&self, id: NodeId) -> Result<Option<Entity>, StoreError> {
        let snapshot = self.graph.read();
        match snapshot.node_properties(id) {
            Some(props) => Ok(Some(entity::from_properties(props)?)),
            None => Ok(None),
        }
    }

    /// Read an entity by its domain id from a fresh snapshot. `Entity.id` is indexed, so
    /// this is a probe — summarization (M2.T06) uses it to name a committed fact's subject
    /// and entity-typed objects when it rolls a subject's facts into a note.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the stored data cannot be decoded into an [`Entity`].
    pub fn entity_by_id(&self, id: &Id) -> Result<Option<Entity>, StoreError> {
        let snapshot = self.graph.read();
        let label = db_string(Entity::LABEL)?;
        let prop = db_string("id")?;
        let value = crate::convert::id_value(id)?;
        let Some(rows) = snapshot.nodes_with_property_eq(&label, &prop, &value) else {
            return Ok(None);
        };
        for row in rows.iter() {
            let Some(node) = snapshot.node_id_for_row(selene_graph::RowIndex::new(row)) else {
                continue;
            };
            if let Some(props) = snapshot.node_properties(node) {
                return Ok(Some(entity::from_properties(props)?));
            }
        }
        Ok(None)
    }

    /// The node ids of every fact whose subject is `subject`, from a fresh snapshot.
    ///
    /// `Fact.subject_id` is scalar-indexed, so this is a probe, not a scan. The
    /// high-precision retrieval path (M2.T08) uses it to turn the entities a query
    /// mentions into a bounded fact candidate seed it composes with the
    /// `current_support_facts` set (03 §4). Returns all matching facts regardless of
    /// status; scoping to current is the caller's set-algebra step.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the subject id cannot be encoded for the lookup.
    pub fn facts_by_subject(&self, subject: &Id) -> Result<Vec<NodeId>, StoreError> {
        let snapshot = self.graph.read();
        let label = db_string(Fact::LABEL)?;
        let prop = db_string("subject_id")?;
        let value = crate::convert::id_value(subject)?;
        let Some(rows) = snapshot.nodes_with_property_eq(&label, &prop, &value) else {
            return Ok(Vec::new());
        };
        let mut nodes = Vec::new();
        for row in rows.iter() {
            if let Some(node) = snapshot.node_id_for_row(selene_graph::RowIndex::new(row)) {
                nodes.push(node);
            }
        }
        Ok(nodes)
    }

    /// Commit a fact node through the single write funnel, returning its node id.
    ///
    /// Writes only the node; [`Store::assert_fact`] additionally wires the `ABOUT`
    /// edge that carries the fact's bi-temporal validity window.
    ///
    /// # Errors
    /// Returns [`StoreError`] if translation, the mutation, or the commit fails.
    pub fn insert_fact(&self, fact: &Fact) -> Result<NodeId, StoreError> {
        let (labels, props) = fact::to_node(fact)?;
        let mut txn = self.graph.begin_write();
        let node_id = {
            let mut mutator = txn.mutator();
            mutator.create_node(labels, props)?
        };
        txn.commit()?;
        Ok(node_id)
    }

    /// Read a fact back by its node id from a fresh snapshot.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the stored data cannot be decoded into a [`Fact`].
    pub fn fact_by_node_id(&self, id: NodeId) -> Result<Option<Fact>, StoreError> {
        let snapshot = self.graph.read();
        match snapshot.node_properties(id) {
            Some(props) => Ok(Some(fact::from_properties(props)?)),
            None => Ok(None),
        }
    }

    /// Read a summary note back by its node id from a fresh snapshot (M2.T06).
    ///
    /// # Errors
    /// Returns [`StoreError`] if the stored data cannot be decoded into a [`Note`].
    pub fn note_by_node_id(&self, id: NodeId) -> Result<Option<Note>, StoreError> {
        let snapshot = self.graph.read();
        match snapshot.node_properties(id) {
            Some(props) => Ok(Some(crate::note::from_properties(props)?)),
            None => Ok(None),
        }
    }

    /// Read the `ABOUT` validity window of a fact node, if it has one.
    ///
    /// The four-timestamp window lives on the edge, not the node (02 §4.2), so this is
    /// how a caller inspects a fact's bi-temporal validity.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the edge cannot be decoded into [`About`].
    pub fn fact_about(&self, fact: NodeId) -> Result<Option<About>, StoreError> {
        let snapshot = self.graph.read();
        let about_label = db_string(About::LABEL)?;
        let Some(adjacency) = snapshot.outgoing_edges(fact) else {
            return Ok(None);
        };
        match adjacency.iter_label(&about_label).next() {
            Some(edge) => match snapshot.edge_properties(edge.edge_id) {
                Some(props) => Ok(Some(fact::about_from_properties(props)?)),
                None => Ok(None),
            },
            None => Ok(None),
        }
    }

    /// Assert a fact and wire its `ABOUT` edge to the subject entity (04 §4).
    ///
    /// One atomic commit: the `Fact` node plus `Fact -ABOUT-> Entity` carrying the
    /// bi-temporal validity window. The subject entity must already exist (the closed
    /// graph validates the `ABOUT` endpoint is an `:Entity`). Returns the fact's id.
    ///
    /// # Errors
    /// Returns [`StoreError::Invariant`] if the `ABOUT` window's bounds are out of
    /// order, or [`StoreError`] if translation, a mutation, or the commit fails;
    /// nothing is published if any step fails.
    pub fn assert_fact(
        &self,
        fact: &Fact,
        subject: NodeId,
        about: &About,
    ) -> Result<NodeId, StoreError> {
        // Fail closed: never persist a window whose bounds are out of order (02 §5).
        if !about.temporal.windows_ordered() {
            return Err(StoreError::invariant(
                "ABOUT validity window bounds are out of order".to_string(),
            ));
        }
        let (labels, props) = fact::to_node(fact)?;
        let about_label = db_string(About::LABEL)?;
        let about_props = fact::about_props(about)?;
        let mut txn = self.graph.begin_write();
        let fact_id = {
            let mut mutator = txn.mutator();
            let fact_id = mutator.create_node(labels, props)?;
            mutator.create_edge(about_label, fact_id, subject, about_props)?;
            fact_id
        };
        txn.commit()?;
        Ok(fact_id)
    }

    /// Supersede `old` by `new`, non-destructively (04 §2–§4).
    ///
    /// One atomic commit that preserves the prior fact: it closes the old fact's
    /// `ABOUT` event-time window (`valid_to` <- the supersession `valid_from`), writes
    /// `old -SUPERSEDED_BY-> new`, and mirrors `old.status = superseded`. The old fact
    /// node and its data remain, so an "as of" query before the supersession instant
    /// still sees it as current (02 §4.2). The transaction-time window
    /// (`ingested_at`/`expired_at`) is deliberately left untouched: the substrate still
    /// holds and believes the record, it is simply no longer event-current.
    ///
    /// # Errors
    /// Returns [`StoreError::Invariant`] if the supersession edge's window is out of
    /// order or its instant precedes the prior fact's `valid_from` (which would close
    /// the window backwards); [`StoreError`] if `old` has no `ABOUT` edge, or if a
    /// mutation or the commit fails. Nothing is published if any check or step fails.
    pub fn supersede_fact(
        &self,
        old: NodeId,
        new: NodeId,
        edge: &SupersededBy,
    ) -> Result<(), StoreError> {
        // One atomic commit, sharing the same mutator-scoped body the consolidation flip
        // uses (materialize::apply_supersession), so the direct-write and consolidation
        // paths can never drift apart. Nothing is published if it errors (txn rolls back).
        let mut txn = self.graph.begin_write();
        {
            let mut mutator = txn.mutator();
            crate::materialize::apply_supersession(&mut mutator, old, new, edge)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Record that `source` contradicts `target`, non-destructively (04 §2).
    ///
    /// One atomic commit: writes `source -CONTRADICTS-> target` and, when
    /// `quarantine_source` is set, mirrors `source.status = quarantined`. Both facts
    /// and their data remain; a live `CONTRADICTS` edge removes the source from the
    /// current-support set via the provider (02 §9), and quarantined facts are excluded
    /// from default current retrieval but retained and auditable (04 §2).
    ///
    /// # Errors
    /// Returns [`StoreError::Invariant`] if the contradiction edge's window is out of
    /// order, or [`StoreError`] if a mutation or the commit fails.
    pub fn contradict_fact(
        &self,
        source: NodeId,
        target: NodeId,
        edge: &Contradicts,
        quarantine_source: bool,
    ) -> Result<(), StoreError> {
        // Shares materialize::apply_contradiction with the consolidation flip (see
        // supersede_fact). Nothing is published if it errors (the txn rolls back).
        let mut txn = self.graph.begin_write();
        {
            let mut mutator = txn.mutator();
            crate::materialize::apply_contradiction(
                &mut mutator,
                source,
                target,
                edge,
                quarantine_source,
            )?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Commit a capture bundle through the single mutation funnel (04 §1).
    ///
    /// Writes the episode, its provenance record, and the capture audit event as one
    /// atomic commit, wiring `Episode -HAS_PROVENANCE-> ProvenanceRecord` and
    /// `AuditEvent -AUDIT-> Episode`. The caller has already set each record's
    /// `subject_id`/`actor_id` to the episode's domain id; the edges connect the
    /// freshly assigned node ids. Durable before visible, like every write here.
    ///
    /// # Errors
    /// Returns [`StoreError`] if translation, any node/edge mutation, or the commit
    /// fails; nothing is published if any step fails.
    pub fn commit_capture(
        &self,
        episode: &Episode,
        provenance: &ProvenanceRecord,
        audit: &AuditEvent,
    ) -> Result<CaptureWriteIds, StoreError> {
        let (episode_labels, episode_props) = episode::to_node(episode)?;
        let (provenance_labels, provenance_props) = provenance::to_node(provenance)?;
        let (audit_labels, audit_props) = audit::to_node(audit)?;
        let has_provenance = db_string(HasProvenance::LABEL)?;
        let audit_edge = db_string(Audit::LABEL)?;

        let mut txn = self.graph.begin_write();
        let ids = {
            let mut mutator = txn.mutator();
            let episode_id = mutator.create_node(episode_labels, episode_props)?;
            let provenance_id = mutator.create_node(provenance_labels, provenance_props)?;
            mutator.create_edge(
                has_provenance,
                episode_id,
                provenance_id,
                PropertyMap::from_pairs(Vec::new())?,
            )?;
            let audit_id = mutator.create_node(audit_labels, audit_props)?;
            mutator.create_edge(
                audit_edge,
                audit_id,
                episode_id,
                PropertyMap::from_pairs(Vec::new())?,
            )?;
            CaptureWriteIds {
                episode: episode_id,
                provenance: provenance_id,
                audit: audit_id,
            }
        };
        txn.commit()?;
        Ok(ids)
    }

    /// Read a provenance record back by its node id (for tests and inspection).
    ///
    /// # Errors
    /// Returns [`StoreError`] if the stored data cannot be decoded.
    pub fn provenance_by_node_id(
        &self,
        id: NodeId,
    ) -> Result<Option<ProvenanceRecord>, StoreError> {
        match self.graph.read().node_properties(id) {
            Some(props) => Ok(Some(provenance::from_properties(props)?)),
            None => Ok(None),
        }
    }

    /// Read an audit event back by its node id (for tests and inspection).
    ///
    /// # Errors
    /// Returns [`StoreError`] if the stored data cannot be decoded.
    pub fn audit_event_by_node_id(&self, id: NodeId) -> Result<Option<AuditEvent>, StoreError> {
        match self.graph.read().node_properties(id) {
            Some(props) => Ok(Some(audit::from_properties(props)?)),
            None => Ok(None),
        }
    }
}

/// The node ids assigned by a [`Store::commit_capture`] write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureWriteIds {
    /// The committed episode.
    pub episode: NodeId,
    /// The provenance record proving the write.
    pub provenance: NodeId,
    /// The capture audit event.
    pub audit: NodeId,
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
