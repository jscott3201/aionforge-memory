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
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode};
use aionforge_domain::nodes::forensic::AuditEvent;
use aionforge_domain::time::Timestamp;
use selene_core::{
    DbString, LabelDiff, LabelSet, NodeId, PropertyDiff, PropertyMap, Value, db_string,
};
use selene_graph::{RowIndex, SeleneGraph};
use serde_json::Value as JsonValue;

use crate::convert::{
    as_id, as_str, as_timestamp, enum_from_value, enum_value, id_value, json_from_value,
    json_value, key, namespace_value, string_value, timestamp_value,
};
use crate::error::StoreError;
use crate::gql::{BoundQuery, QueryResult};
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
    /// ULID, so the pair is a stable total order over the commit stream.
    #[must_use]
    pub fn watermark_for(episode: &Episode) -> String {
        format!(
            "{}|{}",
            episode.identity.ingested_at,
            episode.identity.id.as_str()
        )
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LagSnapshot {
    /// The `captured_at` of the oldest episode still needing consolidation, if any.
    pub oldest_pending_captured_at: Option<Timestamp>,
    /// How many episodes are `raw` or `in_progress`.
    pub episodes_pending: u64,
    /// How many episodes are `failed`.
    pub episodes_failed: u64,
    /// The current graph generation (the commit-stream watermark).
    pub generation: u64,
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
        // Commit order over the bounded pending set: ingested_at, then the time-ordered
        // ULID as the tiebreak.
        items.sort_by(|a, b| {
            a.episode
                .identity
                .ingested_at
                .cmp(&b.episode.identity.ingested_at)
                .then_with(|| {
                    a.episode
                        .identity
                        .id
                        .as_str()
                        .cmp(b.episode.identity.id.as_str())
                })
        });
        items.truncate(limit);
        Ok(items)
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

    /// Mark `episode` consolidated and advance the cursor in one atomic commit
    /// (write-and-consolidation §3 idempotency).
    ///
    /// The flip is guarded: the episode must still be in `expected` state under the
    /// write lock, or the commit is refused (a no-op), so a racing second consumer
    /// cannot double-apply. Because the state change and the cursor advance land in
    /// the *same* commit, a crash leaves them consistent — either the episode is
    /// `new_state` and the cursor is advanced, or neither happened and the next pass
    /// re-runs the still-`raw` episode from scratch. Returns the new graph generation.
    ///
    /// # Errors
    /// Returns [`StoreError::Invariant`] if the episode is not in `expected` state, or
    /// [`StoreError`] if the mutation or commit fails. Nothing is published on error.
    pub fn commit_consolidation_episode(
        &self,
        node_id: NodeId,
        expected: ConsolidationState,
        new_state: ConsolidationState,
        cursor: &ConsolidationCursor,
        now: &Timestamp,
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

    /// Record a `consolidation_failed` audit event, optionally marking the episode
    /// `failed`, in one commit (write-and-consolidation §3).
    ///
    /// The cursor is deliberately *not* advanced — it holds at the failure until the
    /// episode is resolved or skipped. A transient failure leaves the episode `raw`
    /// (retried next tick); a fatal failure marks it `failed` (excluded from the
    /// queue, retained and auditable). The audit event is written unsigned, mirroring
    /// the non-signed capture path.
    ///
    /// # Errors
    /// Returns [`StoreError`] if translation, a mutation, or the commit fails.
    pub fn record_consolidation_failure(
        &self,
        episode_node_id: NodeId,
        audit_event: &AuditEvent,
        mark_failed: bool,
    ) -> Result<(), StoreError> {
        let (audit_labels, audit_props) = audit::to_node(audit_event)?;
        let audit_edge = db_string(Audit::LABEL)?;
        let mut txn = self.graph().begin_write();
        {
            let mut mutator = txn.mutator();
            let audit_id = mutator.create_node(audit_labels, audit_props)?;
            mutator.create_edge(
                audit_edge,
                audit_id,
                episode_node_id,
                PropertyMap::from_pairs(Vec::new())?,
            )?;
            if mark_failed {
                mutator.update_node(
                    episode_node_id,
                    LabelDiff::new([], [])?,
                    PropertyDiff::new(
                        [(
                            db_string(CONSOLIDATION_STATE)?,
                            enum_value(&ConsolidationState::Failed)?,
                        )],
                        [],
                    )?,
                )?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Snapshot the consolidation backlog for the lag metric (write-and-consolidation §3).
    ///
    /// Reads the oldest pending `captured_at` and the pending/failed counts plus the
    /// current generation. The lag *duration* is computed above this layer against the
    /// caller's clock.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a query fails or a stored value cannot be decoded.
    pub fn consolidation_lag(&self) -> Result<LagSnapshot, StoreError> {
        // Oldest pending captured_at + pending count, in one ordered read.
        let pending = BoundQuery::new(
            "MATCH (e:Episode) WHERE e.consolidation_state IN [$raw, $in_progress] \
             RETURN e.captured_at AS captured_at ORDER BY e.captured_at ASC",
        )
        .bind("raw", enum_value(&ConsolidationState::Raw)?)?
        .bind("in_progress", enum_value(&ConsolidationState::InProgress)?)?;
        let (oldest_pending_captured_at, episodes_pending) = match self.execute(&pending)? {
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
            oldest_pending_captured_at,
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
