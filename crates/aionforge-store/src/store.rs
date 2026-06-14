//! The L0 store: a single owned `SharedGraph` with typed read/write and
//! parameter-bound GQL.

use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use aionforge_domain::edges::{About, Contradicts, SupersededBy};
use aionforge_domain::embedding::Embedding;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::agent::Agent;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::episodic::Episode;
use aionforge_domain::nodes::semantic::{Entity, Fact};
use aionforge_domain::time::Timestamp;
use selene_core::{GraphId, NodeId, Value, db_string};
use selene_gql::{CallPlanCache, SharedPlanCache};
use selene_graph::{DEFAULT_WAL_FILE_NAME, GraphTypeDef, SeleneGraph, SharedGraph, WalConfig};

use crate::config::StoreConfig;
use crate::convert::{as_id, as_namespace, id_value};
use crate::error::StoreError;
use crate::gql::{BoundQuery, QueryResult};
use crate::providers::candidate_state_provider;
use crate::search::SearchKind;
use crate::{agent, entity, episode, fact};

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
    /// The substrate audit-event signer (06 §6, M4.T06). Empty (the default) leaves every
    /// audit write's signature exactly as the author built it — byte-identical to the
    /// pre-signing store. Once installed, the audit write funnel (`audit::ensure_event`)
    /// and the capture append stamp each blank-signature event at commit time, so no
    /// author site can forget to sign: the funnel is the only door. A `OnceLock` because
    /// the installer (the engine, when `sign_audit_events` is enabled) only holds
    /// `Arc<Store>` at that point — and because install-once makes a mid-life signer swap
    /// (two signers across one store's life, a determinism hazard) structurally impossible.
    audit_signer:
        std::sync::OnceLock<std::sync::Arc<dyn aionforge_domain::verify::AuditEventSigner>>,
    /// Shared per-graph GQL plan caches, built once and cloned into every session, so
    /// the fixed-source request plans are parsed and lowered once and reused across the
    /// many short-lived sessions. See [`crate::plan_cache`].
    pub(crate) shared_plan_cache: Arc<SharedPlanCache>,
    pub(crate) call_plan_cache: Arc<CallPlanCache>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // SharedGraph is not Debug, so we do not recurse into it.
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// File name of the write-ahead log inside a durable store directory.
    pub const WAL_FILE_NAME: &'static str = DEFAULT_WAL_FILE_NAME;

    /// Install the substrate audit-event signer (M4.T06 PR-5g), once per store life.
    /// Every subsequent audit write whose signature is blank is stamped at commit time
    /// inside the write funnel. Takes `&self` so the engine can install through its
    /// `Arc<Store>` at the point it reads `sign_audit_events`.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a signer is already installed — two signers across one
    /// store's life would break the deterministic re-sign story, so this fails loudly.
    pub fn install_audit_signer(
        &self,
        signer: std::sync::Arc<dyn aionforge_domain::verify::AuditEventSigner>,
    ) -> Result<(), StoreError> {
        self.audit_signer.set(signer).map_err(|_| {
            StoreError::invariant("an audit signer is already installed on this store".to_string())
        })
    }

    /// The installed audit signer, if audit signing is enabled.
    pub(crate) fn audit_signer(&self) -> Option<&dyn aionforge_domain::verify::AuditEventSigner> {
        self.audit_signer.get().map(std::sync::Arc::as_ref)
    }

    /// Assemble a store over an opened graph, building its per-store plan caches.
    ///
    /// Every constructor funnels through here so the cache pair and the audit-signer
    /// latch are initialized identically regardless of how the graph was opened.
    fn assemble(graph: SharedGraph, config: StoreConfig) -> Self {
        let (shared_plan_cache, call_plan_cache) = crate::plan_cache::new_plan_caches();
        Self {
            graph,
            config,
            audit_signer: std::sync::OnceLock::new(),
            shared_plan_cache,
            call_plan_cache,
        }
    }

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
        Ok(Self::assemble(graph, config))
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
        create_locked_store_dir(dir)?;
        let graph = SharedGraph::builder(graph_id())
            .with_wal(dir.join(DEFAULT_WAL_FILE_NAME), WalConfig::default())?
            .bound_to(empty_graph_type()?)?
            .with_provider(candidate_state_provider()?)
            .build()?;
        Ok(Self::assemble(graph, config))
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
    /// but it does reconcile drifted vector-index kinds to the catalog (the interim
    /// greenfield-tax fix; non-lossy, dimension-preserving) and re-run the §13.5
    /// dimension-consistency check and the audit-signature latch check (02 §4.11), all of
    /// which the version-guarded [`Store::migrate`] would skip on an already-current
    /// graph.
    ///
    /// # Errors
    /// Returns [`StoreError`] if recovery fails (corrupt or mismatched persistence,
    /// type drift, a recovered vector index whose dimension disagrees with `config`,
    /// or a replayed schema that still declares `AuditEvent.signature` immutable).
    pub fn recover(dir: &Path, config: StoreConfig) -> Result<Self, StoreError> {
        vet_locked_store_dir(dir)?;
        let graph = SharedGraph::recover_closed_with_providers(
            dir,
            graph_id(),
            empty_graph_type()?,
            vec![candidate_state_provider()?],
        )
        .map_err(StoreError::from_recovery)?;
        let store = Self::assemble(graph, config);
        // Converge any vector index whose kind drifted from the catalog (e.g. a store
        // written before the all-TurboQuant default) by dropping and recreating it at the
        // catalog kind — non-lossy, the engine backfills from the primary vectors. Run
        // BEFORE the dimension check so it stays dimension-preserving and a real
        // embedder-dimension change still fails loudly. This removes the index-kind slice
        // of the greenfield tax: a kind-only change converges on open, no fresh store.
        let reconciled = store.reconcile_vector_index_kinds(config.embedding_dimension)?;
        emit_index_kind_reconciliation(&reconciled);
        store.dimension_consistency_check(config.embedding_dimension)?;
        store.audit_signature_latch_check()?;
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
        let started = Instant::now();
        let mode = if dir.join(DEFAULT_WAL_FILE_NAME).exists() {
            "recover"
        } else {
            "fresh"
        };
        let result = if mode == "recover" {
            Self::recover(dir, config)
        } else {
            Self::open_persistent_migrated(dir, config, now)
        };
        emit_open_metrics(mode, &result, started.elapsed());
        result
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

    /// Read an episode by its domain id from a fresh snapshot.
    ///
    /// This returns live and soft-forgotten episodes; caller-facing read surfaces must
    /// still enforce visibility and `expired_at` policy before rendering the value.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the id lookup or stored episode decode fails.
    pub fn episode_by_id(&self, id: &Id) -> Result<Option<Episode>, StoreError> {
        let snapshot = self.graph.read();
        let Some(node) = crate::convert::node_by_id(&snapshot, Episode::LABEL, id)? else {
            return Ok(None);
        };
        match snapshot.node_properties(node) {
            Some(props) => Ok(Some(episode::from_properties(props)?)),
            None => Ok(None),
        }
    }

    /// Live episodes for a session id, ordered by ingestion and capped by `limit`.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the lookup or episode decode fails.
    pub fn live_episodes_by_session_id(
        &self,
        session_id: &Id,
        limit: usize,
    ) -> Result<Vec<Episode>, StoreError> {
        let snapshot = self.graph.read();
        let label = db_string(Episode::LABEL)?;
        let prop = db_string("session_id")?;
        let value = id_value(session_id)?;
        let Some(rows) = snapshot.nodes_with_property_eq(&label, &prop, &value) else {
            return Ok(Vec::new());
        };
        let mut episodes = Vec::new();
        for row in rows.iter() {
            let Some(node) = snapshot.node_id_for_row(selene_graph::RowIndex::new(row)) else {
                continue;
            };
            let Some(props) = snapshot.node_properties(node) else {
                continue;
            };
            let episode = episode::from_properties(props)?;
            if episode.identity.expired_at.is_none() {
                episodes.push(episode);
            }
        }
        episodes.sort_by(|left, right| {
            left.identity
                .ingested_at
                .to_string()
                .cmp(&right.identity.ingested_at.to_string())
                .then_with(|| {
                    left.identity
                        .id
                        .to_string()
                        .cmp(&right.identity.id.to_string())
                })
        });
        episodes.truncate(limit);
        Ok(episodes)
    }

    /// The newest live episode that explicitly supersedes `id`, if one exists.
    ///
    /// # Errors
    /// Returns [`StoreError`] if episode decode fails.
    pub fn live_episode_superseded_by(&self, id: &Id) -> Result<Option<Id>, StoreError> {
        let mut by_target = self.live_episode_superseded_by_many([id])?;
        Ok(by_target.remove(id))
    }

    /// The newest live episode that explicitly supersedes each target id.
    ///
    /// This is the recall-scale reverse lookup for episode supersession metadata. It
    /// scans the live episode label set once, records only claims whose `origin.supersedes`
    /// points at one of the requested targets, and keeps the newest replacement per target.
    /// Callers that need annotations for many candidates should use this instead of N
    /// calls to [`Store::live_episode_superseded_by`].
    ///
    /// # Errors
    /// Returns [`StoreError`] if episode decode fails.
    pub fn live_episode_superseded_by_many<'a>(
        &self,
        ids: impl IntoIterator<Item = &'a Id>,
    ) -> Result<HashMap<Id, Id>, StoreError> {
        let targets: HashSet<Id> = ids.into_iter().copied().collect();
        if targets.is_empty() {
            return Ok(HashMap::new());
        }
        let snapshot = self.graph.read();
        let label = db_string(Episode::LABEL)?;
        let Some(rows) = snapshot.nodes_with_label(&label) else {
            return Ok(HashMap::new());
        };
        let mut newest: HashMap<Id, Episode> = HashMap::new();
        for row in rows.iter() {
            let Some(node) = snapshot.node_id_for_row(selene_graph::RowIndex::new(row)) else {
                continue;
            };
            let Some(props) = snapshot.node_properties(node) else {
                continue;
            };
            let episode = episode::from_properties(props)?;
            if episode.identity.expired_at.is_some() {
                continue;
            }
            let Some(target) = episode.origin.as_ref().and_then(|origin| origin.supersedes) else {
                continue;
            };
            if !targets.contains(&target) {
                continue;
            }
            match newest.entry(target) {
                Entry::Vacant(slot) => {
                    slot.insert(episode);
                }
                Entry::Occupied(mut slot) => {
                    if episode_is_newer(&episode, slot.get()) {
                        slot.insert(episode);
                    }
                }
            }
        }
        Ok(newest
            .into_iter()
            .map(|(target, replacement)| (target, replacement.identity.id))
            .collect())
    }

    /// Whether any episode — live or soft-forgotten — already carries this domain id.
    ///
    /// The signed-write collision pre-check (06 §3, M4.T03): a signed write adopts a
    /// host-supplied subject id as its episode id. `Episode.id` is `UNIQUE` in the DDL
    /// (`catalog`), enforced at commit, so a duplicate id can never actually land — a second
    /// `commit_capture` with the same id fails the write. This probe sits in front of that
    /// constraint: on the common path it lets the capture path reject a reused id with a clean,
    /// audited collision *before* spending an embedder round-trip and before the commit fails
    /// with an opaque store error. (The exact-duplicate probe is no help here — it keys on
    /// `content_hash`, not `id`, so it misses a reused id over different content.) `Episode.id`
    /// is scalar-indexed, so this is a probe, not a scan, and the index must exist for the
    /// probe to mean anything: `nodes_with_property_eq` returns `None` (read here as "absent")
    /// when no index is registered, so without it the pre-check silently no-ops and the
    /// commit-time `UNIQUE` is the only line left. Soft-forgotten episodes (`expired_at` set)
    /// still hold their id, so they count: the id stays taken.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the id cannot be encoded for the lookup.
    pub fn episode_exists(&self, id: &Id) -> Result<bool, StoreError> {
        let snapshot = self.graph.read();
        let label = db_string(Episode::LABEL)?;
        let prop = db_string("id")?;
        let value = crate::convert::id_value(id)?;
        let Some(rows) = snapshot.nodes_with_property_eq(&label, &prop, &value) else {
            return Ok(false);
        };
        for row in rows.iter() {
            let Some(node) = snapshot.node_id_for_row(selene_graph::RowIndex::new(row)) else {
                continue;
            };
            if snapshot.node_properties(node).is_some() {
                return Ok(true);
            }
        }
        Ok(false)
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

    /// The namespace of the live episode with this domain id, if one exists.
    ///
    /// The supersedes-hint validation probe (04 §1 step 3): the capture path checks a
    /// writer-claimed target id resolves to a *live* episode (a soft-forgotten one is
    /// already out of recall, so superseding it is moot) and reads its namespace so the
    /// writer's authority over it can be checked. `Episode.id` is scalar-indexed, so
    /// this is a probe, not a scan. Returns `Ok(None)` for missing AND soft-forgotten
    /// alike — the caller collapses every miss to one outcome so the probe is no
    /// existence oracle.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the query fails or the stored namespace cannot parse.
    pub fn episode_namespace_by_id(&self, id: &Id) -> Result<Option<Namespace>, StoreError> {
        let query = BoundQuery::new(
            "MATCH (e:Episode) WHERE e.id = $id AND e.expired_at IS NULL \
             RETURN e.namespace AS ns LIMIT 1",
        )
        .bind_uuid("id", id)?;
        match self.execute(&query)? {
            QueryResult::Rows(rows) => match rows.value(0, 0) {
                Some(value) => Ok(Some(as_namespace(value)?)),
                None => Ok(None),
            },
            _ => Ok(None),
        }
    }

    /// How many live episodes in `namespace` carry this exact `content_hash`, capped at
    /// `window` (05 §1, M3.T06).
    ///
    /// The reuse-evidence probe behind conservative skill induction: a procedure an agent
    /// re-emitted byte-for-byte is counted by its `content_hash`, an indexed `STRING` column, so
    /// this is an index probe, not a scan. The `LIMIT $window` bounds the work a high-volume
    /// agent can ask of one consolidation pass and is the only count that matters (the induction
    /// threshold is far below the window). Only active episodes count — a soft-forgotten one
    /// (`expired_at` set, 02 §3) is not evidence — and the `namespace` filter keeps the count
    /// inside the agent-private trust boundary the induced skill will live in.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the query fails.
    pub fn count_recent_episodes_by_content_hash(
        &self,
        namespace: &Namespace,
        content_hash: &ContentHash,
        window: usize,
    ) -> Result<usize, StoreError> {
        let query = BoundQuery::new(
            "MATCH (e:Episode) WHERE e.content_hash = $h AND e.namespace = $ns \
             AND e.expired_at IS NULL RETURN e.id AS id LIMIT $w",
        )
        .bind_str("h", content_hash.as_str())?
        .bind_str("ns", &namespace.to_string())?
        .bind("w", Value::Uint(window as u64))?;
        match self.execute(&query)? {
            QueryResult::Rows(rows) => Ok(rows.row_count()),
            _ => Ok(0),
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
    /// The domain ids of facts this episode supports, via its outgoing `SUPPORTS` edges.
    ///
    /// The supersedes-hint consumption probe (04 §1 step 3): consolidation resolves a
    /// hinted episode to the facts it evidences, so the hint can widen supersession
    /// detection to exactly those incumbents. `Episode.id` is scalar-indexed and the
    /// traversal is one hop from a single node. Returns an empty list for a missing
    /// episode or one that supports nothing — both mean "the hint touches no facts".
    ///
    /// # Errors
    /// Returns [`StoreError`] if the query fails or an id cannot decode.
    pub fn fact_ids_supported_by_episode(&self, episode: &Id) -> Result<Vec<Id>, StoreError> {
        let query = BoundQuery::new(
            "MATCH (e:Episode)-[:SUPPORTS]->(f:Fact) WHERE e.id = $id RETURN f.id AS id",
        )
        .bind_uuid("id", episode)?;
        match self.execute(&query)? {
            QueryResult::Rows(rows) => (0..rows.row_count())
                .filter_map(|i| rows.value(i, 0))
                .map(as_id)
                .collect(),
            _ => Ok(Vec::new()),
        }
    }

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

    /// Enroll an agent: commit its node through the single write funnel, returning the
    /// node id. `Agent.id` is unique, so an agent is registered once and the substrate
    /// stores only its public key. Provenance verification (M4.T03) resolves a writer's
    /// key by agent id through [`Store::agent_by_id`] before checking a write's signature.
    ///
    /// # Errors
    /// Returns [`StoreError`] if translation, the mutation, or the commit fails.
    pub fn create_agent(&self, agent: &Agent) -> Result<NodeId, StoreError> {
        let (labels, props) = agent::to_node(agent)?;
        let mut txn = self.graph.begin_write();
        let node_id = {
            let mut mutator = txn.mutator();
            mutator.create_node(labels, props)?
        };
        txn.commit()?;
        Ok(node_id)
    }

    /// Read an agent by its domain id from a fresh snapshot. `Agent.id` is unique-indexed,
    /// so this is a probe, not a scan — the signing gate uses it to resolve a writer's
    /// public key before verifying the write's provenance signature.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the stored data cannot be decoded into an [`Agent`].
    pub fn agent_by_id(&self, id: &Id) -> Result<Option<Agent>, StoreError> {
        let snapshot = self.graph.read();
        let label = db_string(Agent::LABEL)?;
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
                return Ok(Some(agent::from_properties(props)?));
            }
        }
        Ok(None)
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
}

fn episode_is_newer(candidate: &Episode, current: &Episode) -> bool {
    let candidate_key = (
        candidate.identity.ingested_at.to_string(),
        candidate.identity.id.to_string(),
    );
    let current_key = (
        current.identity.ingested_at.to_string(),
        current.identity.id.to_string(),
    );
    candidate_key > current_key
}

fn emit_open_metrics(mode: &'static str, result: &Result<Store, StoreError>, elapsed: Duration) {
    let outcome = if result.is_ok() { "success" } else { "error" };
    metrics::counter!(
        "aionforge_store_open_total",
        "mode" => mode,
        "outcome" => outcome,
    )
    .increment(1);
    metrics::histogram!(
        "aionforge_store_open_duration_seconds",
        "mode" => mode,
        "outcome" => outcome,
    )
    .record(elapsed.as_secs_f64());
    // Complement the metrics with a tracing event so the store-open lifecycle is visible in the
    // logs too (logging hot-paths, task #9 PR2). Low-cardinality only: mode (fresh|recover),
    // outcome, and elapsed — no path, no data. Success at info, failure at warn.
    let elapsed_ms = elapsed.as_millis() as u64;
    if result.is_ok() {
        tracing::info!(target: "aionforge::store", mode, outcome, elapsed_ms, "store opened");
    } else {
        tracing::warn!(target: "aionforge::store", mode, outcome, elapsed_ms, "store open failed");
    }
}

/// Record the recovery-time vector-index kind reconciliation: a counter per converged
/// index plus a human-readable line, so the drop-and-recreate is observable (the
/// "auto-reconcile + metric" policy). Silent when nothing drifted.
fn emit_index_kind_reconciliation(reconciled: &[crate::indexes::VectorKindReconciliation]) {
    for row in reconciled {
        metrics::counter!(
            "aionforge_store_vector_index_kind_reconciled_total",
            "label" => row.label.clone(),
            "property" => row.property.clone(),
            "to_kind" => row.to_kind.clone(),
        )
        .increment(1);
        tracing::info!(
            label = %row.label,
            property = %row.property,
            from_kind = %row.from_kind,
            to_kind = %row.to_kind,
            "reconciled vector index kind to catalog on open (non-lossy rebuild)"
        );
    }
}

#[cfg(unix)]
fn create_locked_store_dir(dir: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::DirBuilderExt;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .or_else(|err| {
            if err.kind() == std::io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(err)
            }
        })
        .map_err(|err| {
            StoreError::persist(format!(
                "cannot create the store directory {}: {err}",
                dir.display()
            ))
        })?;
    vet_locked_store_dir(dir)
}

#[cfg(not(unix))]
fn create_locked_store_dir(dir: &Path) -> Result<(), StoreError> {
    std::fs::create_dir_all(dir).map_err(|err| {
        StoreError::persist(format!(
            "cannot create the store directory {}: {err}",
            dir.display()
        ))
    })
}

#[cfg(unix)]
fn vet_locked_store_dir(dir: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;

    let link_meta = std::fs::symlink_metadata(dir).map_err(|err| {
        StoreError::persist(format!(
            "cannot inspect the store directory {}: {err}",
            dir.display()
        ))
    })?;
    if link_meta.file_type().is_symlink() {
        return Err(StoreError::persist(format!(
            "store directory {} is a symlink; use a real owner-only directory",
            dir.display()
        )));
    }
    let meta = std::fs::metadata(dir).map_err(|err| {
        StoreError::persist(format!(
            "cannot inspect the store directory {}: {err}",
            dir.display()
        ))
    })?;
    if !meta.is_dir() {
        return Err(StoreError::persist(format!(
            "store path {} is not a directory",
            dir.display()
        )));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(StoreError::persist(format!(
            "store directory {} has mode {mode:o}, refusing anything looser than 0700",
            dir.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn vet_locked_store_dir(_dir: &Path) -> Result<(), StoreError> {
    Ok(())
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
