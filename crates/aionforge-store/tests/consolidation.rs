//! Integration tests for the L0 consolidation surface (M2.T03, write-and-consolidation
//! §2–§3): the durable cursor, work discovery, the crash-safe state-flip, the
//! in-progress reset hook, failure recording, and the lag snapshot.

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::Timestamp;
use aionforge_store::{
    BoundQuery, ConsolidationArtifacts, ConsolidationCursor, NodeId, QueryResult, Store,
    StoreConfig,
};
use serde_json::json;

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

/// The empty derived-artifact set: these tests exercise the flip/cursor mechanics, not
/// materialization (which has its own coverage in `materialize.rs` / the engine tests).
fn no_artifacts() -> ConsolidationArtifacts {
    ConsolidationArtifacts::default()
}

fn now() -> Timestamp {
    ts("2026-06-06T12:00:00-05:00[America/Chicago]")
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

/// Build and insert a `raw` episode at the given ingested/captured instants; return its
/// node id and the domain value.
fn insert_episode(
    store: &Store,
    content: &str,
    ingested_at: &str,
    captured_at: &str,
) -> (NodeId, Episode) {
    let episode = Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts(ingested_at),
            namespace: Namespace::Agent("alice".to_string()),
            expired_at: None,
        },
        stats: stats(),
        content: content.to_string(),
        role: Role::User,
        captured_at: ts(captured_at),
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    let node_id = store.insert_episode(&episode).expect("insert episode");
    (node_id, episode)
}

/// A cursor advanced over `episode`, as the scheduler would build it.
fn cursor_at(episode: &Episode) -> ConsolidationCursor {
    ConsolidationCursor {
        last_position: ConsolidationCursor::watermark_for(episode),
        last_episode_id: Some(episode.identity.id),
        last_processed_at: Some(now()),
        rule_versions: json!({ "noop": 1 }),
    }
}

fn episode_state(store: &Store, node_id: NodeId) -> ConsolidationState {
    store
        .episode_by_node_id(node_id)
        .expect("read episode")
        .expect("episode present")
        .consolidation_state
}

#[test]
fn cursor_is_absent_until_the_first_flip_then_round_trips() {
    let store = store();
    assert!(
        store.load_consolidation_cursor().expect("load").is_none(),
        "no cursor exists until the first episode is consolidated"
    );

    let (node_id, episode) = insert_episode(
        &store,
        "first",
        "2026-06-06T09:00:00-05:00[America/Chicago]",
        "2026-06-06T08:59:00-05:00[America/Chicago]",
    );
    let cursor = cursor_at(&episode);
    store
        .commit_consolidation_episode(
            node_id,
            ConsolidationState::Raw,
            ConsolidationState::Consolidated,
            &cursor,
            &now(),
            &no_artifacts(),
        )
        .expect("flip");

    let loaded = store
        .load_consolidation_cursor()
        .expect("load")
        .expect("cursor now exists");
    assert_eq!(loaded, cursor, "the cursor round-trips through the flip");
}

#[test]
fn discovery_is_oldest_first_skips_consolidated_and_respects_the_limit() {
    let store = store();
    let (n1, e1) = insert_episode(
        &store,
        "one",
        "2026-06-06T09:00:00-05:00[America/Chicago]",
        "2026-06-06T09:00:00-05:00[America/Chicago]",
    );
    let (_n2, e2) = insert_episode(
        &store,
        "two",
        "2026-06-06T09:01:00-05:00[America/Chicago]",
        "2026-06-06T09:01:00-05:00[America/Chicago]",
    );
    let (_n3, e3) = insert_episode(
        &store,
        "three",
        "2026-06-06T09:02:00-05:00[America/Chicago]",
        "2026-06-06T09:02:00-05:00[America/Chicago]",
    );

    // Oldest first, limited.
    let batch = store.discover_consolidation_work(2).expect("discover");
    let ids: Vec<Id> = batch.iter().map(|w| w.episode.identity.id).collect();
    assert_eq!(
        ids,
        vec![e1.identity.id, e2.identity.id],
        "discovery returns the two oldest raw episodes in commit order"
    );

    // Consolidate the oldest; it drops out of the queue, the rest shift up.
    store
        .commit_consolidation_episode(
            n1,
            ConsolidationState::Raw,
            ConsolidationState::Consolidated,
            &cursor_at(&e1),
            &now(),
            &no_artifacts(),
        )
        .expect("flip e1");
    let remaining: Vec<Id> = store
        .discover_consolidation_work(10)
        .expect("discover")
        .iter()
        .map(|w| w.episode.identity.id)
        .collect();
    assert_eq!(
        remaining,
        vec![e2.identity.id, e3.identity.id],
        "a consolidated episode is never rediscovered"
    );
}

#[test]
fn reset_in_progress_returns_episodes_to_raw() {
    let store = store();
    let (node_id, episode) = insert_episode(
        &store,
        "claimed",
        "2026-06-06T09:00:00-05:00[America/Chicago]",
        "2026-06-06T09:00:00-05:00[America/Chicago]",
    );
    // A pass that crashed mid-flight would leave the episode in_progress.
    store
        .commit_consolidation_episode(
            node_id,
            ConsolidationState::Raw,
            ConsolidationState::InProgress,
            &cursor_at(&episode),
            &now(),
            &no_artifacts(),
        )
        .expect("claim");
    assert_eq!(
        episode_state(&store, node_id),
        ConsolidationState::InProgress
    );

    assert_eq!(store.reset_in_progress_episodes().expect("reset"), 1);
    assert_eq!(
        episode_state(&store, node_id),
        ConsolidationState::Raw,
        "a reset in-progress episode is raw again"
    );
    assert_eq!(
        store.reset_in_progress_episodes().expect("reset"),
        0,
        "the reset is idempotent"
    );
}

#[test]
fn begin_consolidation_guard_refuses_a_wrong_expected_state() {
    let store = store();
    let (node_id, _episode) = insert_episode(
        &store,
        "guarded-begin",
        "2026-06-06T09:00:00-05:00[America/Chicago]",
        "2026-06-06T09:00:00-05:00[America/Chicago]",
    );
    // The episode is raw, but we claim it is already consolidated: the begin flip is refused.
    let result = store.begin_consolidation_episode(node_id, ConsolidationState::Consolidated);
    assert!(
        result.is_err(),
        "the begin guard rejects a wrong expected state"
    );
    assert_eq!(
        episode_state(&store, node_id),
        ConsolidationState::Raw,
        "a refused begin leaves the episode raw, not in_progress"
    );
}

#[test]
fn flip_guard_refuses_a_wrong_expected_state() {
    let store = store();
    let (node_id, episode) = insert_episode(
        &store,
        "guarded",
        "2026-06-06T09:00:00-05:00[America/Chicago]",
        "2026-06-06T09:00:00-05:00[America/Chicago]",
    );
    // The episode is raw, but we claim it is already consolidated: refused.
    let result = store.commit_consolidation_episode(
        node_id,
        ConsolidationState::Consolidated,
        ConsolidationState::Consolidated,
        &cursor_at(&episode),
        &now(),
        &no_artifacts(),
    );
    assert!(result.is_err(), "the guard rejects a wrong expected state");
    assert_eq!(
        episode_state(&store, node_id),
        ConsolidationState::Raw,
        "nothing changed — the episode is still raw"
    );
    assert!(
        store.load_consolidation_cursor().expect("load").is_none(),
        "a refused flip publishes no cursor"
    );
}

#[test]
fn flip_marks_consolidated_and_advances_the_generation() {
    let store = store();
    let before = store.consolidation_lag().expect("lag").generation;
    let (node_id, episode) = insert_episode(
        &store,
        "work",
        "2026-06-06T09:00:00-05:00[America/Chicago]",
        "2026-06-06T09:00:00-05:00[America/Chicago]",
    );
    let after_insert = store.consolidation_lag().expect("lag").generation;
    assert!(after_insert > before, "the insert advanced the generation");

    let new_generation = store
        .commit_consolidation_episode(
            node_id,
            ConsolidationState::Raw,
            ConsolidationState::Consolidated,
            &cursor_at(&episode),
            &now(),
            &no_artifacts(),
        )
        .expect("flip");
    assert!(
        new_generation > after_insert,
        "the flip advanced the generation again"
    );
    assert_eq!(
        episode_state(&store, node_id),
        ConsolidationState::Consolidated
    );
}

#[test]
fn lag_reports_the_oldest_pending_and_drains_as_work_completes() {
    let store = store();
    let (n1, e1) = insert_episode(
        &store,
        "oldest",
        "2026-06-06T09:00:00-05:00[America/Chicago]",
        "2026-06-06T08:00:00-05:00[America/Chicago]",
    );
    let (_n2, _e2) = insert_episode(
        &store,
        "newer",
        "2026-06-06T09:05:00-05:00[America/Chicago]",
        "2026-06-06T08:30:00-05:00[America/Chicago]",
    );

    let lag = store.consolidation_lag().expect("lag");
    assert_eq!(lag.episodes_pending, 2);
    assert_eq!(lag.episodes_failed, 0);
    assert_eq!(
        lag.oldest_pending_ingested_at,
        Some(ts("2026-06-06T09:00:00-05:00[America/Chicago]")),
        "the oldest pending ingested_at drives backlog age",
    );

    store
        .commit_consolidation_episode(
            n1,
            ConsolidationState::Raw,
            ConsolidationState::Consolidated,
            &cursor_at(&e1),
            &now(),
            &no_artifacts(),
        )
        .expect("flip oldest");
    let lag = store.consolidation_lag().expect("lag");
    assert_eq!(lag.episodes_pending, 1, "the backlog drained by one");
    assert_eq!(
        lag.oldest_pending_ingested_at,
        Some(ts("2026-06-06T09:05:00-05:00[America/Chicago]")),
        "the next-oldest ingestion is now the age driver",
    );
}

#[test]
fn failure_records_an_audit_without_advancing_the_cursor() {
    let store = store();
    let (node_id, _episode) = insert_episode(
        &store,
        "doomed",
        "2026-06-06T09:00:00-05:00[America/Chicago]",
        "2026-06-06T09:00:00-05:00[America/Chicago]",
    );
    // Transient: no state change, no cursor advance, but the audit is recorded.
    store
        .record_consolidation_failure(node_id, &failure_audit(&_episode.identity.id), false)
        .expect("record transient failure");
    assert_eq!(
        episode_state(&store, node_id),
        ConsolidationState::Raw,
        "a transient failure leaves the episode raw for retry"
    );
    assert!(
        store.load_consolidation_cursor().expect("load").is_none(),
        "a failure does not advance the cursor"
    );
    assert_eq!(
        consolidation_failed_count(&store),
        1,
        "the audit event landed"
    );

    // Fatal: mark the episode failed (out of the queue), still no cursor advance.
    store
        .record_consolidation_failure(node_id, &failure_audit(&_episode.identity.id), true)
        .expect("record fatal failure");
    assert_eq!(episode_state(&store, node_id), ConsolidationState::Failed);
    assert_eq!(store.consolidation_lag().expect("lag").episodes_failed, 1);
    assert_eq!(
        consolidation_failed_count(&store),
        2,
        "each failure records its own audit event"
    );
    assert!(
        store.load_consolidation_cursor().expect("load").is_none(),
        "a fatal failure still does not advance the cursor"
    );
}

#[test]
fn recording_the_same_failure_attempt_twice_is_idempotent() {
    // The scheduler's failure-audit id is content-derived from (episode, attempt), and
    // `AuditEvent.id` is UNIQUE — so a re-record of the same attempt (a retried tick, a partial
    // crash) must reuse the existing node rather than collide. `record_consolidation_failure`
    // is dedup-aware, so the second call is a no-op, not a UNIQUE violation or a double-count.
    let store = store();
    let (node_id, episode) = insert_episode(
        &store,
        "doomed",
        "2026-06-06T09:00:00-05:00[America/Chicago]",
        "2026-06-06T09:00:00-05:00[America/Chicago]",
    );
    let audit = AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(b"consolidation_failed|fixed-episode|1"),
            ingested_at: now(),
            namespace: Namespace::System,
            expired_at: None,
        },
        kind: AuditKind::ConsolidationFailed,
        subject_id: episode.identity.id,
        actor_id: Id::from_content_hash(b"scheduler"),
        payload: json!({ "pass": "noop", "reason": "boom", "attempts": 1 }),
        signature: String::new(),
        occurred_at: now(),
    };
    store
        .record_consolidation_failure(node_id, &audit, false)
        .expect("first record");
    store
        .record_consolidation_failure(node_id, &audit, false)
        .expect("re-record must reuse the node, not collide on the UNIQUE id");
    assert_eq!(
        consolidation_failed_count(&store),
        1,
        "re-recording the same attempt reuses the audit node — no duplicate, no UNIQUE violation"
    );
    assert_eq!(
        store
            .count_consolidation_failures(&episode.identity.id)
            .expect("count"),
        1,
        "the persistent attempt count is not double-counted on a re-record"
    );
}

/// A fresh `consolidation_failed` audit event about `subject` (each gets a unique id).
fn failure_audit(subject: &Id) -> AuditEvent {
    AuditEvent {
        identity: Identity {
            id: Id::generate(),
            ingested_at: now(),
            namespace: Namespace::System,
            expired_at: None,
        },
        kind: AuditKind::ConsolidationFailed,
        subject_id: *subject,
        actor_id: Id::generate(),
        payload: json!({ "pass": "noop", "reason": "boom" }),
        signature: String::new(),
        occurred_at: now(),
    }
}

/// Count `consolidation_failed` audit events via a parameter-bound query.
fn consolidation_failed_count(store: &Store) -> usize {
    let query = BoundQuery::new("MATCH (a:AuditEvent) WHERE a.kind = $kind RETURN a.id AS id")
        .bind_str("kind", "consolidation_failed")
        .expect("bind kind");
    match store.execute(&query).expect("count audits") {
        QueryResult::Rows(rows) => rows.row_count(),
        other => panic!("unexpected result: {other:?}"),
    }
}
