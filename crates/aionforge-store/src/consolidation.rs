//! The L0 surface the asynchronous consolidator drives (write-and-consolidation
//! spec §2–§3, M2.T03).
//!
//! The consolidator is a background loop that turns raw episodes into derived
//! memory. This module gives it four things and nothing more: a durable
//! [`ConsolidationCursor`] singleton it can read and advance, a way to discover the
//! episodes still needing work, a crash-safe state-flip that advances the cursor in
//! the same commit as the episode it marks, and an observability snapshot for lag.
//!
//! The episode's own `consolidation_state` is the unit of progress (the work queue):
//! `raw → consolidated` (or `→ failed`). The cursor is the durable *record* of how
//! far the loop has gotten — advanced atomically with each flip so the two can never
//! disagree. Discovery is driven by `consolidation_state`, so a restart resumes from
//! the surviving raw episodes rather than reprocessing consolidated ones; the cursor
//! persists across that restart and reports the resume point. The actual derivation
//! rules (extract, resolve, supersede, summarize) live above this layer (M2.T04+) and
//! plug into the scheduler; this module never decides *what* a pass does, only that
//! the episode is marked and the cursor advanced exactly once, durably.

use aionforge_domain::edges::Audit;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::procedural::{BadPattern, Skill};
use aionforge_domain::nodes::semantic::{Entity, Fact};
use aionforge_domain::nodes::work::{WorkItem, WorkStatus};
use aionforge_domain::time::Timestamp;
use selene_core::{
    DbString, LabelDiff, LabelSet, NodeId, PropertyDiff, PropertyMap, Value, db_string,
};
use selene_graph::{RowIndex, SeleneGraph};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::convert::{
    as_id, as_str, as_timestamp, enum_from_value, enum_value, id_value, json_from_value,
    json_value, key, namespace_value, string_value, timestamp_value,
};
use crate::error::StoreError;
use crate::gql::{BoundQuery, QueryResult};
use crate::materialize::{ConsolidationArtifacts, ensure_edge, materialize_into};
use crate::store::Store;
use crate::{audit, episode};

/// The selene-db node label for the consolidation cursor singleton.
const CURSOR_LABEL: &str = "ConsolidationCursor";
// Cursor columns (catalog §; identity block reused from the shared block).
const ID: &str = "id";
const INGESTED_AT: &str = "ingested_at";
const NAMESPACE: &str = "namespace";
const LAST_POSITION: &str = "last_position";
const LAST_EPISODE_ID: &str = "last_episode_id";
const LAST_PROCESSED_AT: &str = "last_processed_at";
const RULE_VERSIONS: &str = "rule_versions";
const CONSOLIDATION_STATE: &str = "consolidation_state";

/// The durable consolidation cursor (data-model §; write-and-consolidation §2–§3).
///
/// A singleton, like `SchemaVersion`. `last_position` is the resumable watermark,
/// serialized as `"<ingested_at_rfc3339>|<episode_id>"` — a total order over the
/// commit stream (both halves sort by creation time), advanced as episodes are
/// consolidated. An empty `last_position` means nothing has been consolidated yet.
/// `rule_versions` records the `{pass_name: version}` in force at the watermark; it is
/// recorded only in M2.T03 (version-mismatch reprocessing is a later task).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsolidationCursor {
    /// The resumable watermark (`"ingested_at|episode_id"`, or empty at the start).
    pub last_position: String,
    /// The id of the last episode the cursor advanced over (denormalized for logs).
    pub last_episode_id: Option<Id>,
    /// Wall-clock of the last successful advance (feeds lag staleness).
    pub last_processed_at: Option<Timestamp>,
    /// The `{pass_name: version}` snapshot in force at `last_position`.
    pub rule_versions: JsonValue,
}

impl Default for ConsolidationCursor {
    fn default() -> Self {
        Self {
            last_position: String::new(),
            last_episode_id: None,
            last_processed_at: None,
            rule_versions: JsonValue::Object(serde_json::Map::new()),
        }
    }
}

impl ConsolidationCursor {
    /// The watermark string for an episode: `"<ingested_at_rfc3339>|<id>"`.
    ///
    /// `ingested_at` is the immutable commit-time stamp and `id` is a time-ordered
    /// UUIDv7, so the pair is a stable total order over the commit stream.
    #[must_use]
    pub fn watermark_for(episode: &Episode) -> String {
        format!("{}|{}", episode.identity.ingested_at, episode.identity.id)
    }
}

/// One unit of consolidation work: an episode that still needs a pass.
///
/// Carries the engine [`NodeId`] (resolved once at discovery) so the crash-safe flip
/// can mark exactly this node without a second lookup; the domain [`Episode`] is the
/// read-only view a pass reasons over.
#[derive(Debug, Clone)]
pub struct ConsolidationWorkItem {
    /// The episode's engine node id, for the state-flip.
    pub node_id: NodeId,
    /// The episode itself.
    pub episode: Episode,
}

/// An observability snapshot of the consolidation backlog (write-and-consolidation §3).
///
/// Carries the raw primary values; the lag *duration* is derived above this layer
/// against an injected clock, since L0 keeps no ambient clock for stored time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LagSnapshot {
    /// The `ingested_at` of the oldest episode still needing consolidation, if any.
    pub oldest_pending_ingested_at: Option<Timestamp>,
    /// How many episodes are `raw` or `in_progress`.
    pub episodes_pending: u64,
    /// How many episodes are `failed`.
    pub episodes_failed: u64,
    /// The current graph generation (the commit-stream watermark).
    pub generation: u64,
}

/// The selene-db labels that carry a live memory — the canonical public list of the
/// memory-bearing kinds (episodic, semantic, associative, procedural). Cursors, schema
/// versions, audit/provenance, agents, sessions, and anchors are not memories and are
/// deliberately excluded. [`Store::memory_counts`] counts exactly these kinds.
pub const MEMORY_LABELS: [&str; 6] = [
    Episode::LABEL,
    Fact::LABEL,
    Entity::LABEL,
    Note::LABEL,
    Skill::LABEL,
    BadPattern::LABEL,
];

/// A live per-kind memory census (global operator telemetry).
///
/// Every field counts only *live* nodes of that kind — rows whose `expired_at` is
/// unset, so soft-forgotten memories (which keep their row) are excluded. The fields
/// are taken against one pinned snapshot so they and [`MemoryCounts::total`] are
/// mutually consistent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryCounts {
    /// Live episodes (episodic tier).
    pub episodes: u64,
    /// Live facts (semantic tier).
    pub facts: u64,
    /// Live entities (semantic tier).
    pub entities: u64,
    /// Live notes (associative tier).
    pub notes: u64,
    /// Live skills (procedural tier).
    pub skills: u64,
    /// Live bad patterns (procedural tier).
    pub bad_patterns: u64,
}

impl MemoryCounts {
    /// Total live memories across every kind.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.episodes + self.facts + self.entities + self.notes + self.skills + self.bad_patterns
    }
}

/// A live per-status work-item census (global operator telemetry) — the work-tracking
/// counterpart to [`MemoryCounts`].
///
/// Every field counts only *live* work items of that status (rows whose `expired_at` is unset),
/// taken against one pinned snapshot so the per-status fields and [`WorkCounts::total`] agree.
/// This is deliberately SEPARATE from the memory census: work items are exempt from forgetting
/// and are never counted as memories, so a work item never moves [`MemoryCounts::total`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorkCounts {
    /// Items awaiting a start (`todo`).
    pub todo: u64,
    /// Items underway (`in_progress`).
    pub in_progress: u64,
    /// Items blocked on a dependency (`blocked`).
    pub blocked: u64,
    /// Completed items (`done`).
    pub done: u64,
    /// Abandoned items (`dropped`).
    pub dropped: u64,
}

impl WorkCounts {
    /// Total live work items across every status.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.todo + self.in_progress + self.blocked + self.done + self.dropped
    }
}

impl Store {
    /// Read the consolidation cursor singleton, if it has been created yet.
    ///
    /// `Ok(None)` means no episode has been consolidated yet (the cursor node is
    /// created lazily on the first advance); a caller treats that as the empty
    /// watermark — start from the beginning.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a stored cursor cannot be decoded.
    pub fn load_consolidation_cursor(&self) -> Result<Option<ConsolidationCursor>, StoreError> {
        let snapshot = self.graph().read();
        let Some(node_id) = cursor_node_id(&snapshot)? else {
            return Ok(None);
        };
        let props = snapshot
            .node_properties(node_id)
            .ok_or_else(|| StoreError::decode("consolidation cursor node has no properties"))?;
        Ok(Some(cursor_from_properties(props)?))
    }

    /// Discover up to `limit` episodes still needing consolidation, oldest first.
    ///
    /// The queue is the episode's own `consolidation_state` (`raw`/`in_progress`), so
    /// this resumes naturally after a restart — consolidated episodes are never
    /// returned again. The `consolidation_state` index serves the filter, so the read
    /// touches only the pending set, not every episode; that bounded set is ordered by
    /// `(ingested_at, id)` in memory (the engine cannot index `ZONED DATETIME`) so work
    /// runs in commit order and the cursor advances monotonically. The engine node id
    /// comes straight off the index probe, so the later flip needs no second lookup.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a stored episode cannot be decoded.
    pub fn discover_consolidation_work(
        &self,
        limit: usize,
    ) -> Result<Vec<ConsolidationWorkItem>, StoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let snapshot = self.graph().read();
        let label = db_string(Episode::LABEL)?;
        let state_key = db_string(CONSOLIDATION_STATE)?;
        let mut items: Vec<ConsolidationWorkItem> = Vec::new();
        // Probe the `consolidation_state` index for each pending state, then hydrate the
        // node id + episode directly off the matched rows.
        for state in [ConsolidationState::Raw, ConsolidationState::InProgress] {
            let value = enum_value(&state)?;
            let Some(rows) = snapshot.nodes_with_property_eq(&label, &state_key, &value) else {
                continue;
            };
            for row in rows.iter() {
                let Some(node_id) = snapshot.node_id_for_row(RowIndex::new(row)) else {
                    // The index row resolved to no live node: an index/store inconsistency.
                    // Skip it but surface it — a silently dropped row would stall the queue.
                    tracing::warn!(
                        row,
                        "consolidation discovery: index row resolves to no node"
                    );
                    continue;
                };
                let Some(props) = snapshot.node_properties(node_id) else {
                    tracing::warn!(
                        ?node_id,
                        "consolidation discovery: episode node has no properties"
                    );
                    continue;
                };
                items.push(ConsolidationWorkItem {
                    node_id,
                    episode: episode::from_properties(props)?,
                });
            }
        }
        // Commit order over the bounded pending set: ingested_at, then the UUIDv7 id
        // as the tiebreak.
        items.sort_by(|a, b| {
            a.episode
                .identity
                .ingested_at
                .cmp(&b.episode.identity.ingested_at)
                .then_with(|| a.episode.identity.id.cmp(&b.episode.identity.id))
        });
        items.truncate(limit);
        Ok(items)
    }

    /// Mark an episode `in_progress` before its passes run, guarded on its `expected` state.
    ///
    /// The scheduler calls this before applying passes so in-flight work is observable and a
    /// crash mid-pass leaves a visible `in_progress` marker (cleaned up by
    /// [`Self::reset_in_progress_episodes`] at startup). The flip is guarded — the episode must
    /// still be in `expected` state under the write lock — so it composes with the same
    /// exactly-once discipline as the terminal flip; re-marking an already-`in_progress` episode
    /// (a direct re-tick without a startup reset) is an idempotent no-op.
    ///
    /// # Errors
    /// Returns [`StoreError::Invariant`] if the episode is not in `expected` state, or
    /// [`StoreError`] if the mutation or commit fails.
    pub fn begin_consolidation_episode(
        &self,
        episode_node_id: NodeId,
        expected: ConsolidationState,
    ) -> Result<(), StoreError> {
        let mut txn = self.graph().begin_write();
        {
            let mut mutator = txn.mutator();
            let current: ConsolidationState = {
                let read = mutator.read();
                let props = read
                    .node_properties(episode_node_id)
                    .ok_or_else(|| StoreError::decode("episode to begin no longer exists"))?;
                enum_from_value(props.get(&db_string(CONSOLIDATION_STATE)?).ok_or_else(|| {
                    StoreError::decode("episode has no consolidation_state property")
                })?)?
            };
            if current != expected {
                return Err(StoreError::invariant(format!(
                    "episode is {current:?}, expected {expected:?} to begin consolidation"
                )));
            }
            mutator.update_node(
                episode_node_id,
                LabelDiff::new([], [])?,
                PropertyDiff::new(
                    [(
                        db_string(CONSOLIDATION_STATE)?,
                        enum_value(&ConsolidationState::InProgress)?,
                    )],
                    [],
                )?,
            )?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Reset every `in_progress` episode back to `raw` (crash-recovery hook).
    ///
    /// An episode is only `in_progress` if a pass was interrupted before its terminal
    /// flip; resetting makes the next pass re-run it cleanly. Idempotent — a second
    /// call resets nothing. Returns how many were reset (a startup log line).
    ///
    /// # Errors
    /// Returns [`StoreError`] if the update fails.
    pub fn reset_in_progress_episodes(&self) -> Result<u64, StoreError> {
        let query = BoundQuery::new(
            "MATCH (e:Episode) WHERE e.consolidation_state = $in_progress \
             SET e.consolidation_state = $raw RETURN e.id AS id",
        )
        .bind("in_progress", enum_value(&ConsolidationState::InProgress)?)?
        .bind("raw", enum_value(&ConsolidationState::Raw)?)?;
        // A `SET ... RETURN` is a data-modifying statement, so it auto-commits and yields
        // a `Written` outcome carrying the matched rows.
        match self.execute(&query)? {
            QueryResult::Written { rows, .. } => Ok(rows.map_or(0, |rows| rows.row_count() as u64)),
            QueryResult::Empty => Ok(0),
            QueryResult::Rows(_) => Err(StoreError::decode(
                "reset_in_progress_episodes expected a Written result from SET ... RETURN",
            )),
        }
    }

    /// Materialize the pass artifacts, mark `episode` consolidated, and advance the
    /// cursor in one atomic commit (write-and-consolidation §3 idempotency).
    ///
    /// The flip is guarded: the episode must still be in `expected` state under the
    /// write lock, or the commit is refused (a no-op), so a racing second consumer
    /// cannot double-apply. The derived `artifacts` (facts, entities, and their
    /// `ABOUT`/`MENTIONS`/`SUPPORTS`/`DERIVED_FROM` edges, plus the decision audit
    /// trail) are written in the *same* commit as the flip and cursor advance, so a
    /// crash leaves all three consistent — either the episode is `new_state`, the
    /// derived memory exists, and the cursor is advanced, or none of it happened and the
    /// next pass re-runs the still-`raw` episode from scratch. Materialization is itself
    /// idempotent (content dedup), so a re-run never duplicates. Returns the new graph
    /// generation.
    ///
    /// # Errors
    /// Returns [`StoreError::Invariant`] if the episode is not in `expected` state or a
    /// derived fact's window is out of order, or [`StoreError`] if a mutation or the
    /// commit fails. Nothing is published on error.
    pub fn commit_consolidation_episode(
        &self,
        node_id: NodeId,
        expected: ConsolidationState,
        new_state: ConsolidationState,
        cursor: &ConsolidationCursor,
        now: &Timestamp,
        artifacts: &ConsolidationArtifacts,
    ) -> Result<u64, StoreError> {
        let mut txn = self.graph().begin_write();
        {
            let mut mutator = txn.mutator();

            // Guard: re-read the episode state under the write lock. Reads end here so
            // the borrow is released before the mutations below.
            let current: ConsolidationState = {
                let read = mutator.read();
                let props = read
                    .node_properties(node_id)
                    .ok_or_else(|| StoreError::decode("episode to consolidate no longer exists"))?;
                enum_from_value(props.get(&db_string(CONSOLIDATION_STATE)?).ok_or_else(|| {
                    StoreError::decode("episode has no consolidation_state property")
                })?)?
            };
            if current != expected {
                return Err(StoreError::invariant(format!(
                    "episode is {current:?}, expected {expected:?} to consolidate"
                )));
            }

            // Materialize the derived facts/entities/edges before the flip, so they land
            // in the same commit. Dedup makes a re-run a no-op.
            materialize_into(&mut mutator, node_id, artifacts, now, self.audit_signer())?;

            // Resolve the cursor node id (Copy) before mutating, releasing the borrow.
            let cursor_node = cursor_node_id(mutator.read())?;

            // Flip the episode state.
            mutator.update_node(
                node_id,
                LabelDiff::new([], [])?,
                PropertyDiff::new(
                    [(db_string(CONSOLIDATION_STATE)?, enum_value(&new_state)?)],
                    [],
                )?,
            )?;

            // Advance the cursor singleton in the same commit.
            match cursor_node {
                Some(id) => {
                    mutator.update_node(
                        id,
                        LabelDiff::new([], [])?,
                        cursor_property_diff(cursor)?,
                    )?;
                }
                None => {
                    let (labels, props) = cursor_to_node(cursor, now)?;
                    mutator.create_node(labels, props)?;
                }
            }
        }
        txn.commit()?;
        Ok(self.graph().read().meta.generation)
    }

    /// Record a `consolidation_failed` audit event and settle the episode's state, in one
    /// commit (write-and-consolidation §3).
    ///
    /// The cursor is deliberately *not* advanced — it holds at the failure until the episode is
    /// resolved or skipped. A transient failure returns the episode to `raw` (clearing the
    /// `in_progress` mark the scheduler set before running passes, so it is plainly pending and
    /// retried next tick); a fatal failure marks it `failed` (excluded from the queue, retained
    /// and auditable). The audit event is written unsigned, mirroring the non-signed capture path.
    ///
    /// # Errors
    /// Returns [`StoreError`] if translation, a mutation, or the commit fails.
    pub fn record_consolidation_failure(
        &self,
        episode_node_id: NodeId,
        audit_event: &AuditEvent,
        mark_failed: bool,
    ) -> Result<(), StoreError> {
        let settled = if mark_failed {
            ConsolidationState::Failed
        } else {
            ConsolidationState::Raw
        };
        let mut txn = self.graph().begin_write();
        {
            let mut mutator = txn.mutator();
            // The audit id is content-derived (keyed on episode + attempt), and `AuditEvent.id`
            // is UNIQUE, so the write must dedup: a re-recorded attempt reuses the node and
            // edge rather than colliding on the constraint (mirrors the materialize audit path).
            let audit_node =
                audit::ensure_event(&mut mutator, audit_event, self.audit_signer())?.node;
            ensure_edge(
                &mut mutator,
                Audit::LABEL,
                audit_node,
                episode_node_id,
                PropertyMap::from_pairs(Vec::new())?,
            )?;
            // Settle the episode: failed (terminal) or back to raw (clears any in_progress mark).
            mutator.update_node(
                episode_node_id,
                LabelDiff::new([], [])?,
                PropertyDiff::new(
                    [(db_string(CONSOLIDATION_STATE)?, enum_value(&settled)?)],
                    [],
                )?,
            )?;
        }
        txn.commit()?;
        Ok(())
    }

    /// How many `consolidation_failed` audit events this episode has accrued — the persistent,
    /// crash-surviving retry count the scheduler reads to decide retry vs. fatal.
    ///
    /// An in-memory counter resets every restart, so a poison-pill episode would get a fresh
    /// retry budget after each crash and never escalate; deriving the count from the durable
    /// audit trail makes the budget survive restarts. `AuditEvent.subject_id` and `kind` are
    /// both indexed, so this is a bounded probe over just this episode's failure audits.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the query fails or the count cannot be decoded.
    pub fn count_consolidation_failures(&self, episode_id: &Id) -> Result<u32, StoreError> {
        let query = BoundQuery::new(
            "MATCH (a:AuditEvent) WHERE a.subject_id = $sid AND a.kind = $kind \
             RETURN count(a) AS n",
        )
        .bind("sid", id_value(episode_id)?)?
        .bind("kind", enum_value(&AuditKind::ConsolidationFailed)?)?;
        match self.execute(&query)? {
            QueryResult::Rows(rows) => match rows.value(0, 0) {
                Some(Value::Uint(n)) => Ok(u32::try_from(*n).unwrap_or(u32::MAX)),
                Some(Value::Int(n)) => Ok(u32::try_from(*n).unwrap_or(0)),
                other => Err(StoreError::decode(format!(
                    "consolidation failure count returned a non-integer: {other:?}"
                ))),
            },
            other => Err(StoreError::decode(format!(
                "consolidation failure count returned a non-row result: {other:?}"
            ))),
        }
    }

    /// A live per-kind census of every memory-bearing kind (operator telemetry).
    ///
    /// A node is *live* iff its `expired_at` property is unset: soft-forget keeps the
    /// row and merely stamps `expired_at` (02 §3), so the unset check is the same
    /// liveness predicate as GQL's `IS NULL`. All six labels in [`MEMORY_LABELS`] are
    /// counted against ONE pinned read snapshot, so the per-kind fields and
    /// [`MemoryCounts::total`] are mutually consistent (no row is counted under one
    /// generation and missed under another). This is engine-native — it reads the label
    /// bitmap and subtracts the expired rows — with no bolt-on counter to drift.
    ///
    /// This is an operator-wide census: it takes no principal and counts live nodes
    /// across *all* namespaces and roles, so it is deliberately broader than recall
    /// (which excludes the System namespace and system-role episodes). Only the
    /// aggregate per-kind numbers are surfaced — never content or per-namespace counts —
    /// so it exposes no individual namespace's existence.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a label or property key cannot be interned.
    pub fn memory_counts(&self) -> Result<MemoryCounts, StoreError> {
        // One pinned snapshot for all six labels: the per-kind fields and total() must
        // agree, which they only can if every label is counted against the same version.
        let snapshot = self.graph().read();
        let expired_key = db_string("expired_at")?;
        let mut counts = MemoryCounts::default();
        let count_label = |label_str: &str| -> Result<u64, StoreError> {
            let label = db_string(label_str)?;
            let Some(rows) = snapshot.nodes_with_label(&label) else {
                return Ok(0);
            };
            let mut live = 0u64;
            for row in rows.iter() {
                let Some(node) = snapshot.node_id_for_row(RowIndex::new(row)) else {
                    continue;
                };
                let Some(props) = snapshot.node_properties(node) else {
                    continue;
                };
                // `expired_at` is written only on soft-forget (02 §3); its absence is
                // live, matching GQL `IS NULL` and the store's presence-based liveness
                // convention (forget_read.rs / cooling.rs / forget_write.rs).
                if props.get(&expired_key).is_none() {
                    live += 1;
                }
            }
            Ok(live)
        };
        counts.episodes = count_label(Episode::LABEL)?;
        counts.facts = count_label(Fact::LABEL)?;
        counts.entities = count_label(Entity::LABEL)?;
        counts.notes = count_label(Note::LABEL)?;
        counts.skills = count_label(Skill::LABEL)?;
        counts.bad_patterns = count_label(BadPattern::LABEL)?;
        Ok(counts)
    }

    /// A live per-status census of work items (operator telemetry) — the work-tracking
    /// counterpart to [`Store::memory_counts`].
    ///
    /// Each [`WorkStatus`] bucket is counted against ONE pinned snapshot (so the per-status
    /// fields and [`WorkCounts::total`] are mutually consistent) by probing the `work_status`
    /// scalar index per status and keeping only *live* rows (those whose `expired_at` is unset —
    /// the same liveness predicate as `memory_counts`). This census is deliberately SEPARATE
    /// from the memory census: work items are exempt from forgetting and are never memories, so
    /// `work_counts` and `memory_counts` share no rows and a work item never moves
    /// [`MemoryCounts::total`].
    ///
    /// # Errors
    /// Returns [`StoreError`] if a label or property key cannot be interned.
    pub fn work_counts(&self) -> Result<WorkCounts, StoreError> {
        // One pinned snapshot across all five status buckets, so the fields and total() agree.
        let snapshot = self.graph().read();
        let label = db_string(WorkItem::LABEL)?;
        let status_key = db_string("work_status")?;
        let expired_key = db_string("expired_at")?;
        let count_status = |status: WorkStatus| -> Result<u64, StoreError> {
            let value = enum_value(&status)?;
            let Some(rows) = snapshot.nodes_with_property_eq(&label, &status_key, &value) else {
                return Ok(0);
            };
            let mut live = 0u64;
            for row in rows.iter() {
                let Some(node) = snapshot.node_id_for_row(RowIndex::new(row)) else {
                    continue;
                };
                let Some(props) = snapshot.node_properties(node) else {
                    continue;
                };
                if props.get(&expired_key).is_none() {
                    live += 1;
                }
            }
            Ok(live)
        };
        Ok(WorkCounts {
            todo: count_status(WorkStatus::Todo)?,
            in_progress: count_status(WorkStatus::InProgress)?,
            blocked: count_status(WorkStatus::Blocked)?,
            done: count_status(WorkStatus::Done)?,
            dropped: count_status(WorkStatus::Dropped)?,
        })
    }

    /// Snapshot the consolidation backlog for the lag metric (write-and-consolidation §3).
    ///
    /// Reads the oldest pending `ingested_at` and the pending/failed counts plus the
    /// current generation. The age *duration* is computed above this layer against the
    /// caller's clock so historical backfills do not look like live stuck work.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a query fails or a stored value cannot be decoded.
    pub fn consolidation_lag(&self) -> Result<LagSnapshot, StoreError> {
        // Oldest pending ingested_at + pending count, in one ordered read.
        let pending = BoundQuery::new(
            "MATCH (e:Episode) WHERE e.consolidation_state IN [$raw, $in_progress] \
             RETURN e.ingested_at AS ingested_at ORDER BY e.ingested_at ASC",
        )
        .bind("raw", enum_value(&ConsolidationState::Raw)?)?
        .bind("in_progress", enum_value(&ConsolidationState::InProgress)?)?;
        let (oldest_pending_ingested_at, episodes_pending) = match self.execute(&pending)? {
            QueryResult::Rows(rows) => {
                let oldest = match rows.value(0, 0) {
                    Some(value) => Some(as_timestamp(value)?),
                    None => None,
                };
                (oldest, rows.row_count() as u64)
            }
            other => {
                return Err(StoreError::decode(format!(
                    "consolidation lag pending query returned a non-row result: {other:?}"
                )));
            }
        };

        let failed = BoundQuery::new(
            "MATCH (e:Episode) WHERE e.consolidation_state = $failed RETURN e.id AS id",
        )
        .bind("failed", enum_value(&ConsolidationState::Failed)?)?;
        let episodes_failed = match self.execute(&failed)? {
            QueryResult::Rows(rows) => rows.row_count() as u64,
            other => {
                return Err(StoreError::decode(format!(
                    "consolidation lag failed query returned a non-row result: {other:?}"
                )));
            }
        };

        Ok(LagSnapshot {
            oldest_pending_ingested_at,
            episodes_pending,
            episodes_failed,
            generation: self.graph().read().meta.generation,
        })
    }
}

/// The engine node id of the cursor singleton, if it exists.
fn cursor_node_id(snapshot: &SeleneGraph) -> Result<Option<NodeId>, StoreError> {
    let label = db_string(CURSOR_LABEL)?;
    let Some(rows) = snapshot.nodes_with_label(&label) else {
        return Ok(None);
    };
    Ok(rows
        .iter()
        .find_map(|row| snapshot.node_id_for_row(RowIndex::new(row))))
}

/// The `(labels, properties)` pair to create the cursor singleton.
fn cursor_to_node(
    cursor: &ConsolidationCursor,
    now: &Timestamp,
) -> Result<(LabelSet, PropertyMap), StoreError> {
    let mut pairs: Vec<(DbString, Value)> = Vec::with_capacity(7);
    pairs.push((key(ID)?, id_value(&Id::generate())?));
    pairs.push((key(INGESTED_AT)?, timestamp_value(now)));
    pairs.push((key(NAMESPACE)?, namespace_value(&Namespace::System)?));
    pairs.push((key(LAST_POSITION)?, string_value(&cursor.last_position)?));
    if let Some(episode_id) = &cursor.last_episode_id {
        pairs.push((key(LAST_EPISODE_ID)?, id_value(episode_id)?));
    }
    if let Some(processed_at) = &cursor.last_processed_at {
        pairs.push((key(LAST_PROCESSED_AT)?, timestamp_value(processed_at)));
    }
    pairs.push((key(RULE_VERSIONS)?, json_value(&cursor.rule_versions)?));
    Ok((
        LabelSet::single(db_string(CURSOR_LABEL)?),
        PropertyMap::from_pairs(pairs)?,
    ))
}

/// The property diff to advance an existing cursor singleton.
fn cursor_property_diff(cursor: &ConsolidationCursor) -> Result<PropertyDiff, StoreError> {
    let mut set: Vec<(DbString, Value)> = Vec::with_capacity(4);
    set.push((key(LAST_POSITION)?, string_value(&cursor.last_position)?));
    set.push((key(RULE_VERSIONS)?, json_value(&cursor.rule_versions)?));
    if let Some(episode_id) = &cursor.last_episode_id {
        set.push((key(LAST_EPISODE_ID)?, id_value(episode_id)?));
    }
    if let Some(processed_at) = &cursor.last_processed_at {
        set.push((key(LAST_PROCESSED_AT)?, timestamp_value(processed_at)));
    }
    Ok(PropertyDiff::new(set, [])?)
}

/// Reconstruct a [`ConsolidationCursor`] from its stored property map.
fn cursor_from_properties(props: &PropertyMap) -> Result<ConsolidationCursor, StoreError> {
    let get =
        |name: &str| -> Result<Option<&Value>, StoreError> { Ok(props.get(&db_string(name)?)) };
    let require = |name: &str| -> Result<&Value, StoreError> {
        get(name)?.ok_or_else(|| StoreError::decode(format!("cursor missing property `{name}`")))
    };
    Ok(ConsolidationCursor {
        last_position: as_str(require(LAST_POSITION)?)?.to_owned(),
        last_episode_id: get(LAST_EPISODE_ID)?.map(as_id).transpose()?,
        last_processed_at: get(LAST_PROCESSED_AT)?.map(as_timestamp).transpose()?,
        rule_versions: json_from_value(require(RULE_VERSIONS)?)?,
    })
}

#[cfg(test)]
mod tests {
    //! The liveness filter must hold on every memory kind, not just `Episode`. The store
    //! layer can insert both an `Episode` and a `Fact` directly, so this proves the
    //! `expired_at`-unset predicate excludes soft-forgotten rows on a NON-`Episode` label
    //! too — the one behaviour the per-kind census hangs on.

    use aionforge_domain::blocks::{Identity, Stats};
    use aionforge_domain::ids::{ContentHash, Id};
    use aionforge_domain::namespace::Namespace;
    use aionforge_domain::nodes::episodic::Role;
    use aionforge_domain::nodes::semantic::{Fact, FactStatus};
    use aionforge_domain::value::ObjectValue;

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

    fn stats() -> Stats {
        Stats {
            importance: 0.5,
            trust: 0.8,
            last_access: ts("2026-06-06T10:00:00-05:00[America/Chicago]"),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.1,
            is_pinned: false,
        }
    }

    fn episode(content: &str, expired_at: Option<Timestamp>) -> Episode {
        Episode {
            identity: Identity {
                id: Id::generate(),
                ingested_at: ts("2026-06-06T09:00:00-05:00[America/Chicago]"),
                namespace: Namespace::Agent("alice".to_string()),
                expired_at,
            },
            stats: stats(),
            content: content.to_string(),
            role: Role::User,
            captured_at: ts("2026-06-06T09:00:00-05:00[America/Chicago]"),
            agent_id: Id::generate(),
            session_id: None,
            content_hash: ContentHash::of(content.as_bytes()),
            embedding: None,
            embedder_model: None,
            consolidation_state: ConsolidationState::Raw,
            origin: None,
        }
    }

    fn fact(statement: &str, expired_at: Option<Timestamp>) -> Fact {
        Fact {
            identity: Identity {
                id: Id::generate(),
                ingested_at: ts("2026-06-06T09:00:00-05:00[America/Chicago]"),
                namespace: Namespace::Global,
                expired_at,
            },
            stats: stats(),
            subject_id: Id::from_content_hash(statement.as_bytes()),
            predicate: "asserts".to_string(),
            object: ObjectValue::Text(statement.to_string()),
            confidence: 0.9,
            status: FactStatus::Active,
            statement: statement.to_string(),
            embedding: None,
            embedder_model: None,
            extraction: None,
            cooled_until: None,
        }
    }

    #[test]
    fn memory_counts_excludes_expired_and_nonmemory_labels() {
        let store = store();
        let expired = ts("2026-06-07T12:00:00-05:00[America/Chicago]");

        // Two live + one soft-forgotten Episode (the row survives, `expired_at` stamped).
        store
            .insert_episode(&episode("ep-live-1", None))
            .expect("insert live episode 1");
        store
            .insert_episode(&episode("ep-live-2", None))
            .expect("insert live episode 2");
        store
            .insert_episode(&episode("ep-gone", Some(expired.clone())))
            .expect("insert expired episode");

        // One live + one soft-forgotten Fact — the NON-Episode liveness coverage.
        store
            .insert_fact(&fact("fact-live", None))
            .expect("insert live fact");
        store
            .insert_fact(&fact("fact-gone", Some(expired)))
            .expect("insert expired fact");

        let counts = store.memory_counts().expect("memory counts");

        // Expired rows are excluded on BOTH the Episode and the Fact label.
        assert_eq!(counts.episodes, 2, "only the two live episodes count");
        assert_eq!(counts.facts, 1, "the soft-forgotten fact is excluded");
        // Kinds with no inserted nodes are zero — non-memory labels never leak in.
        assert_eq!(counts.entities, 0);
        assert_eq!(counts.notes, 0);
        assert_eq!(counts.skills, 0);
        assert_eq!(counts.bad_patterns, 0);
        // total() is the consistent sum of the per-kind fields.
        assert_eq!(counts.total(), 3);
        assert_eq!(
            counts.total(),
            counts.episodes
                + counts.facts
                + counts.entities
                + counts.notes
                + counts.skills
                + counts.bad_patterns
        );
    }
}
