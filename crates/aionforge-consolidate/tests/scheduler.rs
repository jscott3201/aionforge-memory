//! Acceptance tests for the consolidation scheduler (M2.T03, write-and-consolidation
//! §2–§3): the cursor persists and resumes, concurrency is bounded, lag is observable,
//! and a crash mid-pass resumes from the last position and never double-applies.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aionforge_consolidate::{
    ConsolidationConfig, Consolidator, NoopPass, STAGE_DETECTION, STAGE_SUMMARIZATION, StageProfile,
};
use aionforge_domain::nodes::episodic::ConsolidationState;
use aionforge_domain::time::Timestamp;
use aionforge_store::{BoundQuery, QueryResult, Store, Value};

use common::*;

// --- Tests ------------------------------------------------------------------------

#[tokio::test]
async fn tick_once_consolidates_pending_episodes_and_advances_the_cursor() {
    let store = in_memory();
    insert_episode(&store, "one", 1);
    insert_episode(&store, "two", 2);
    let last = insert_episode(&store, "three", 3);

    let mut consolidator = Consolidator::with_clock(store.clone(), config(), fixed_clock());
    consolidator.register(Box::new(NoopPass));
    let report = consolidator.tick_once().await.expect("tick");

    assert_eq!(report.consolidated, 3);
    assert_eq!(report.pending_after, 0);
    assert_eq!(
        episode_state(&store, last),
        ConsolidationState::Consolidated
    );

    let cursor = store
        .load_consolidation_cursor()
        .expect("load")
        .expect("cursor advanced");
    assert!(
        cursor.last_position.contains("09:03:00"),
        "the cursor advanced to the last (newest) episode: {}",
        cursor.last_position
    );
    assert_eq!(cursor.rule_versions, serde_json::json!({ "noop": 1 }));
}

#[tokio::test]
async fn consolidation_resumes_after_restart_without_reprocessing() {
    let dir = temp_dir("resume");
    let migrate_at = ts("2026-01-01T00:00:00-06:00[America/Chicago]");

    {
        let store = Arc::new(
            Store::open_persistent_migrated(&dir, store_config(), &migrate_at).expect("open"),
        );
        for (i, content) in ["a", "b", "c", "d"].iter().enumerate() {
            insert_episode(&store, content, i as u32 + 1);
        }
        // batch_size 2 → only the two oldest consolidate this tick.
        let mut consolidator = Consolidator::with_clock(
            store.clone(),
            ConsolidationConfig {
                batch_size: 2,
                ..config()
            },
            fixed_clock(),
        );
        consolidator.register(Box::new(NoopPass));
        let report = consolidator.tick_once().await.expect("tick");
        assert_eq!(report.consolidated, 2);
        assert_eq!(
            pending(&store),
            2,
            "two episodes remain after the bounded tick"
        );
        drop(consolidator);
        drop(store);
    }

    // Recover from the WAL alone and resume.
    let store = Arc::new(Store::recover(&dir, store_config(), &Timestamp::now()).expect("recover"));
    let cursor = store
        .load_consolidation_cursor()
        .expect("load")
        .expect("cursor survived the restart");
    assert!(
        cursor.last_position.contains("09:02:00"),
        "the cursor resumed at the second episode: {}",
        cursor.last_position
    );
    assert_eq!(
        cursor.rule_versions,
        serde_json::json!({ "noop": 1 }),
        "rule_versions survived the WAL recovery"
    );

    let applied = Arc::new(AtomicUsize::new(0));
    let mut consolidator = Consolidator::with_clock(store.clone(), config(), fixed_clock());
    consolidator.register(Box::new(CountingPass {
        applied: applied.clone(),
    }));
    let report = consolidator.tick_once().await.expect("tick");

    assert_eq!(
        report.consolidated, 2,
        "only the two remaining episodes run"
    );
    assert_eq!(
        applied.load(Ordering::SeqCst),
        2,
        "the already-consolidated episodes are not reprocessed"
    );
    assert_eq!(pending(&store), 0, "the backlog is fully drained");
    drop(consolidator);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_tick_is_bounded_by_batch_size_and_processing_is_single_flight() {
    let store = in_memory();
    for i in 1..=10 {
        insert_episode(&store, "ep", i);
    }
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let mut consolidator = Consolidator::with_clock(
        store.clone(),
        ConsolidationConfig {
            batch_size: 4,
            ..config()
        },
        fixed_clock(),
    );
    consolidator.register(Box::new(ConcurrencyProbePass {
        in_flight: in_flight.clone(),
        max_in_flight: max_in_flight.clone(),
    }));

    let report = consolidator.tick_once().await.expect("tick");
    assert_eq!(
        report.consolidated, 4,
        "a tick takes at most batch_size episodes"
    );
    assert_eq!(pending(&store), 6, "the rest wait for later ticks");
    assert_eq!(
        max_in_flight.load(Ordering::SeqCst),
        1,
        "episodes are processed one at a time (single-flight)"
    );

    // The backlog drains over subsequent ticks.
    consolidator.tick_once().await.expect("tick");
    consolidator.tick_once().await.expect("tick");
    assert_eq!(pending(&store), 0, "three bounded ticks drain ten episodes");
}

#[tokio::test]
async fn lag_is_observable_and_drains_as_work_completes() {
    let store = in_memory();
    // Captured well before the fixed clock's noon, so lag is a known positive duration.
    insert_episode(&store, "old", 1);
    insert_episode(&store, "new", 5);

    let mut consolidator = Consolidator::with_clock(store.clone(), config(), fixed_clock());
    consolidator.register(Box::new(NoopPass));

    let lag = consolidator.lag().expect("lag");
    assert_eq!(lag.episodes_pending, 2);
    assert_eq!(lag.episodes_failed, 0);
    // noon minus 09:01 is just under three hours.
    assert!(
        lag.oldest_pending_lag >= Duration::from_secs(2 * 3600),
        "the oldest pending episode drives a multi-hour lag: {:?}",
        lag.oldest_pending_lag
    );

    consolidator.tick_once().await.expect("tick");
    let lag = consolidator.lag().expect("lag");
    assert_eq!(lag.episodes_pending, 0);
    assert_eq!(
        lag.oldest_pending_lag,
        Duration::ZERO,
        "lag is zero once the backlog is empty"
    );
}

#[tokio::test]
async fn a_consolidated_episode_is_never_reapplied() {
    // Exactly-once commit: re-ticking over a consolidated episode does not re-run it.
    let store = in_memory();
    insert_episode(&store, "once", 1);
    let applied = Arc::new(AtomicUsize::new(0));
    let mut consolidator = Consolidator::with_clock(store.clone(), config(), fixed_clock());
    consolidator.register(Box::new(CountingPass {
        applied: applied.clone(),
    }));

    consolidator.tick_once().await.expect("tick");
    assert_eq!(applied.load(Ordering::SeqCst), 1);
    // Re-tick: the episode is consolidated, so it is not rediscovered or re-applied.
    let report = consolidator.tick_once().await.expect("tick");
    assert_eq!(report.consolidated, 0);
    assert_eq!(
        applied.load(Ordering::SeqCst),
        1,
        "a committed episode is never applied a second time"
    );
}

#[tokio::test]
async fn a_failed_pass_retries_at_least_once_but_commits_exactly_once() {
    // At-least-once apply, exactly-once commit: a pass that fails once then succeeds is
    // applied twice but the episode is consolidated a single time.
    let store = in_memory();
    let node = insert_episode(&store, "flaky", 1);
    let applied = Arc::new(AtomicUsize::new(0));
    let mut consolidator = Consolidator::with_clock(store.clone(), config(), fixed_clock());
    consolidator.register(Box::new(FlakyPass {
        applied: applied.clone(),
        fail_times: 1,
    }));

    // Tick 1: the pass fails transiently; the episode stays raw, nothing committed.
    let report = consolidator.tick_once().await.expect("tick");
    assert_eq!(report.retried, 1);
    assert_eq!(report.consolidated, 0);
    assert_eq!(episode_state(&store, node), ConsolidationState::Raw);
    assert!(
        store.load_consolidation_cursor().expect("load").is_none(),
        "a failed episode does not advance the cursor"
    );

    // Tick 2: the pass succeeds; the episode is consolidated exactly once.
    let report = consolidator.tick_once().await.expect("tick");
    assert_eq!(report.consolidated, 1);
    assert_eq!(applied.load(Ordering::SeqCst), 2, "applied twice (retry)");
    assert_eq!(
        episode_state(&store, node),
        ConsolidationState::Consolidated
    );

    // Tick 3: nothing left.
    let report = consolidator.tick_once().await.expect("tick");
    assert_eq!(report.consolidated, 0);
    assert_eq!(
        applied.load(Ordering::SeqCst),
        2,
        "committed, never re-applied"
    );
}

#[tokio::test]
async fn a_mid_batch_failure_holds_the_cursor_at_the_consolidated_prefix() {
    // The cursor must never advance past a held-back failure: when the second of three
    // episodes fails, the tick stops there — the third is not processed and the cursor
    // sits at the first. A later tick (with the failure cleared) drains the rest in order.
    let store = in_memory();
    let n1 = insert_episode(&store, "first", 1);
    let n2 = insert_episode(&store, "second", 2);
    let n3 = insert_episode(&store, "third", 3);

    let mut consolidator = Consolidator::with_clock(
        store.clone(),
        ConsolidationConfig {
            batch_size: 3,
            ..config()
        },
        fixed_clock(),
    );
    consolidator.register(Box::new(FailOnContentPass {
        fail_content: "second".to_string(),
    }));

    let report = consolidator.tick_once().await.expect("tick");
    assert_eq!(
        report.consolidated, 1,
        "only the first episode consolidates"
    );
    assert_eq!(report.retried, 1, "the second is held back");
    assert_eq!(episode_state(&store, n1), ConsolidationState::Consolidated);
    assert_eq!(
        episode_state(&store, n2),
        ConsolidationState::Raw,
        "the failed episode stays raw"
    );
    assert_eq!(
        episode_state(&store, n3),
        ConsolidationState::Raw,
        "the episode after the failure is not processed past it"
    );
    let cursor = store
        .load_consolidation_cursor()
        .expect("load")
        .expect("cursor");
    assert!(
        cursor.last_position.contains("09:01:00"),
        "the cursor holds at the first episode, never jumping past the failure: {}",
        cursor.last_position
    );

    // Clear the failure: a later tick drains the held-back episode and the one after it.
    let mut healthy = Consolidator::with_clock(
        store.clone(),
        ConsolidationConfig {
            batch_size: 3,
            ..config()
        },
        fixed_clock(),
    );
    healthy.register(Box::new(NoopPass));
    let report = healthy.tick_once().await.expect("tick");
    assert_eq!(
        report.consolidated, 2,
        "the tail drains in order on a later tick"
    );
    assert_eq!(pending(&store), 0);
}

#[tokio::test]
async fn transient_failures_escalate_to_fatal_after_max_retries() {
    let store = in_memory();
    let node = insert_episode(&store, "doomed", 1);
    let mut consolidator = Consolidator::with_clock(
        store.clone(),
        ConsolidationConfig {
            max_retries: 2,
            ..config()
        },
        fixed_clock(),
    );
    consolidator.register(Box::new(AlwaysFailPass { fatal: false }));

    // Attempts 1 and 2 are retries (episode stays raw).
    for _ in 0..2 {
        let report = consolidator.tick_once().await.expect("tick");
        assert_eq!(report.retried, 1);
        assert_eq!(episode_state(&store, node), ConsolidationState::Raw);
    }
    // Attempt 3 exceeds max_retries → fatal.
    let report = consolidator.tick_once().await.expect("tick");
    assert_eq!(report.failed, 1);
    assert_eq!(episode_state(&store, node), ConsolidationState::Failed);
    assert_eq!(store.consolidation_lag().expect("lag").episodes_failed, 1);
}

#[tokio::test]
async fn an_episode_is_marked_in_progress_while_its_passes_run() {
    // The scheduler flips the episode to in_progress before any pass runs, so in-flight work is
    // observable; a pass reading its own episode state sees in_progress, not raw.
    let store = in_memory();
    insert_episode(&store, "watch", 1);
    let observed = Arc::new(Mutex::new(None));
    let mut consolidator = Consolidator::with_clock(store.clone(), config(), fixed_clock());
    consolidator.register(Box::new(ObserveStatePass {
        observed: observed.clone(),
    }));

    consolidator.tick_once().await.expect("tick");
    assert_eq!(
        *observed.lock().expect("observed"),
        Some(ConsolidationState::InProgress),
        "the episode is in_progress while its passes run"
    );
}

#[tokio::test]
async fn a_tick_resumes_an_already_in_progress_episode() {
    // An episode left in_progress (a tick that marked it then crashed, before the next startup
    // reset) is still discovered and consolidated: discovery includes in_progress, and re-marking
    // in_progress is an idempotent no-op, so the tick resumes it cleanly.
    let store = in_memory();
    let node = insert_episode(&store, "interrupted", 1);
    store
        .begin_consolidation_episode(node, ConsolidationState::Raw)
        .expect("mark in_progress");
    assert_eq!(episode_state(&store, node), ConsolidationState::InProgress);

    let mut consolidator = Consolidator::with_clock(store.clone(), config(), fixed_clock());
    consolidator.register(Box::new(NoopPass));
    let report = consolidator.tick_once().await.expect("tick");
    assert_eq!(report.consolidated, 1, "the in_progress episode is resumed");
    assert_eq!(
        episode_state(&store, node),
        ConsolidationState::Consolidated
    );
}

#[tokio::test]
async fn a_fatal_failure_settles_from_in_progress_to_failed() {
    // The episode is marked in_progress before the pass runs, and a fatal failure settles it from
    // in_progress to failed (not from raw). The observed mid-run state proves begin ran first.
    let store = in_memory();
    let node = insert_episode(&store, "doomed", 1);
    let observed = Arc::new(Mutex::new(None));
    let mut consolidator = Consolidator::with_clock(store.clone(), config(), fixed_clock());
    consolidator.register(Box::new(ObserveThenFailPass {
        observed: observed.clone(),
        fatal: true,
    }));

    let report = consolidator.tick_once().await.expect("tick");
    assert_eq!(report.failed, 1);
    assert_eq!(
        *observed.lock().expect("observed"),
        Some(ConsolidationState::InProgress),
        "the pass ran while the episode was in_progress"
    );
    assert_eq!(
        episode_state(&store, node),
        ConsolidationState::Failed,
        "a fatal failure settles the in_progress episode to failed"
    );
}

#[tokio::test]
async fn an_interrupted_in_progress_episode_recovers_across_a_restart() {
    // The full crash-recovery loop with durability: an episode left in_progress (a tick that
    // marked it then crashed) survives a WAL restart as in_progress, the startup reset returns it
    // to raw, and a fresh tick rediscovers and consolidates it exactly once.
    let dir = temp_dir("inprogress-recover");
    let migrate_at = ts("2026-01-01T00:00:00-06:00[America/Chicago]");

    let episode_id = {
        let store = Arc::new(
            Store::open_persistent_migrated(&dir, store_config(), &migrate_at).expect("open"),
        );
        let node = insert_episode(&store, "interrupted", 1);
        // Simulate a tick that marked the episode in_progress, then crashed before committing.
        store
            .begin_consolidation_episode(node, ConsolidationState::Raw)
            .expect("mark in_progress");
        assert_eq!(episode_state(&store, node), ConsolidationState::InProgress);
        let id = store
            .episode_by_node_id(node)
            .expect("read")
            .expect("present")
            .identity
            .id;
        drop(store);
        id
    };

    // Recover from the WAL alone: the in_progress marker is durable.
    let store = Arc::new(Store::recover(&dir, store_config(), &Timestamp::now()).expect("recover"));
    assert_eq!(
        store.consolidation_lag().expect("lag").episodes_pending,
        1,
        "the interrupted episode survives the restart, still pending"
    );
    // The startup crash-recovery hook returns it to raw.
    assert_eq!(
        store.reset_in_progress_episodes().expect("reset"),
        1,
        "the interrupted episode is reset to raw on recovery"
    );

    // A fresh tick rediscovers and consolidates it exactly once.
    let applied = Arc::new(AtomicUsize::new(0));
    let mut consolidator = Consolidator::with_clock(store.clone(), config(), fixed_clock());
    consolidator.register(Box::new(CountingPass {
        applied: applied.clone(),
    }));
    let report = consolidator.tick_once().await.expect("tick");
    assert_eq!(report.consolidated, 1, "the recovered episode consolidates");
    assert_eq!(applied.load(Ordering::SeqCst), 1, "applied exactly once");
    assert_eq!(pending(&store), 0, "the backlog is drained");
    // The cursor advanced to the recovered episode.
    let cursor = store
        .load_consolidation_cursor()
        .expect("load")
        .expect("cursor");
    assert!(
        cursor.last_episode_id.as_ref() == Some(&episode_id),
        "the cursor advanced to the recovered episode"
    );
    drop(consolidator);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn transient_failure_attempts_persist_across_restart_and_escalate() {
    // The retry budget is the durable audit trail, not RAM: a poison-pill episode that exhausts
    // its retries across a restart still escalates to fatal, instead of getting a fresh budget
    // each time the process comes back (which an in-memory counter would hand it, looping forever).
    let dir = temp_dir("attempts-persist");
    let migrate_at = ts("2026-01-01T00:00:00-06:00[America/Chicago]");

    {
        let store = Arc::new(
            Store::open_persistent_migrated(&dir, store_config(), &migrate_at).expect("open"),
        );
        let node = insert_episode(&store, "doomed", 1);
        let mut consolidator = Consolidator::with_clock(
            store.clone(),
            ConsolidationConfig {
                max_retries: 2,
                ..config()
            },
            fixed_clock(),
        );
        consolidator.register(Box::new(AlwaysFailPass { fatal: false }));
        // Two transient attempts before the simulated crash; the episode stays raw.
        for _ in 0..2 {
            let report = consolidator.tick_once().await.expect("tick");
            assert_eq!(report.retried, 1);
            assert_eq!(episode_state(&store, node), ConsolidationState::Raw);
        }
        drop(consolidator);
        drop(store);
    }

    // Recover from the WAL alone: the two prior failure audits are durable.
    let store = Arc::new(Store::recover(&dir, store_config(), &Timestamp::now()).expect("recover"));
    let mut consolidator = Consolidator::with_clock(
        store.clone(),
        ConsolidationConfig {
            max_retries: 2,
            ..config()
        },
        fixed_clock(),
    );
    consolidator.register(Box::new(AlwaysFailPass { fatal: false }));
    // A fresh consolidator's in-memory counter would start at zero; the persisted count is 2,
    // so this third attempt exceeds max_retries and escalates to fatal on the first tick back.
    let report = consolidator.tick_once().await.expect("tick");
    assert_eq!(
        report.failed, 1,
        "the persisted attempt count escalates after a restart"
    );
    let lag = store.consolidation_lag().expect("lag");
    assert_eq!(lag.episodes_failed, 1, "the episode is marked failed");
    assert_eq!(lag.episodes_pending, 0, "and is no longer pending");
    drop(consolidator);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn failure_audit_actor_id_is_deterministic_across_construction() {
    // The scheduler's audit actor id is content-derived from the enabled passes' versions, not a
    // per-process random value, so two freshly-built consolidators over the same configuration
    // stamp the same actor id — forensic attribution survives a restart.
    async fn actor_for_a_run() -> String {
        let store = in_memory();
        insert_episode(&store, "broken", 1);
        let mut consolidator = Consolidator::with_clock(store.clone(), config(), fixed_clock());
        consolidator.register(Box::new(AlwaysFailPass { fatal: true }));
        consolidator.tick_once().await.expect("tick");
        let query = BoundQuery::new(
            "MATCH (a:AuditEvent) WHERE a.kind = $k RETURN a.actor_id AS actor LIMIT 1",
        )
        .bind_str("k", "consolidation_failed")
        .expect("bind kind");
        let QueryResult::Rows(rows) = store.execute(&query).expect("actor query") else {
            panic!("expected rows");
        };
        match rows.value(0, 0) {
            // `actor_id` is a UUID-typed column, so the row carries a `Value::Uuid`; render it
            // to a string for the cross-run stability comparison below.
            Some(Value::Uuid(actor)) => actor.to_string(),
            other => panic!("expected an actor id uuid, got {other:?}"),
        }
    }

    let first = actor_for_a_run().await;
    let second = actor_for_a_run().await;
    assert!(!first.is_empty(), "the failure audit records an actor id");
    assert_eq!(
        first, second,
        "the same pass configuration stamps the same scheduler actor id across constructions"
    );
}

#[tokio::test]
async fn a_fatal_failure_marks_the_episode_failed_immediately() {
    let store = in_memory();
    let node = insert_episode(&store, "broken", 1);
    let mut consolidator = Consolidator::with_clock(store.clone(), config(), fixed_clock());
    consolidator.register(Box::new(AlwaysFailPass { fatal: true }));

    let report = consolidator.tick_once().await.expect("tick");
    assert_eq!(report.failed, 1);
    assert_eq!(episode_state(&store, node), ConsolidationState::Failed);
    assert!(
        store.load_consolidation_cursor().expect("load").is_none(),
        "a fatal failure does not advance the cursor"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_background_loop_consolidates_until_shutdown() {
    let store = in_memory();
    insert_episode(&store, "background", 1);
    let mut consolidator = Consolidator::new(
        store.clone(),
        ConsolidationConfig {
            tick_interval: Duration::from_millis(10),
            ..config()
        },
    );
    consolidator.register(Box::new(NoopPass));
    let handle = consolidator.start();

    let mut drained = false;
    for _ in 0..200 {
        if pending(&store) == 0 {
            drained = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    handle.shutdown().await;
    assert!(
        drained,
        "the spawned loop consolidated the episode on its own"
    );
}

#[tokio::test]
async fn a_tick_exposes_the_per_stage_profile_and_distinguishes_zero_from_rejected() {
    // The verbose-profile contract: a stage that ran but saw nothing
    // (candidates_considered == 0) must be distinguishable from a stage that saw candidates
    // and rejected them all (candidates_considered > 0, derived == 0, rejected_by_guard > 0).
    let store = in_memory();
    insert_episode(&store, "one", 1);

    let mut consolidator = Consolidator::with_clock(store.clone(), config(), fixed_clock());
    consolidator.register(Box::new(ProfilingPass {
        stages: vec![
            // Detection ran but had no input.
            StageProfile::enabled(STAGE_DETECTION, 0, 0, 0, 0, 0),
            // Summarization saw three clusters and the guard rejected every one.
            StageProfile::enabled(STAGE_SUMMARIZATION, 3, 0, 0, 0, 3),
        ],
    }));

    let report = consolidator.tick_once().await.expect("tick");
    assert_eq!(report.consolidated, 1);

    let detection = report
        .profile
        .stages()
        .iter()
        .find(|s| s.stage == STAGE_DETECTION)
        .copied()
        .expect("detection stage present in the profile");
    assert!(detection.enabled, "detection ran");
    assert_eq!(
        detection.candidates_considered, 0,
        "detection saw no candidates"
    );
    assert_eq!(detection.rejected_by_guard, 0);

    let summarization = report
        .profile
        .stages()
        .iter()
        .find(|s| s.stage == STAGE_SUMMARIZATION)
        .copied()
        .expect("summarization stage present in the profile");
    assert!(summarization.enabled, "summarization ran");
    assert_eq!(
        summarization.candidates_considered, 3,
        "summarization considered three clusters"
    );
    assert_eq!(summarization.derived, 0, "but wrote no note");
    assert_eq!(
        summarization.rejected_by_guard, 3,
        "because the detail-retention guard rejected all three"
    );

    // The two outcomes are genuinely distinct, which is the whole point of the profile.
    assert_ne!(
        detection.candidates_considered, summarization.candidates_considered,
        "ran-but-empty differs from saw-candidates-then-rejected"
    );
}

#[tokio::test]
async fn the_profile_accumulates_counts_across_a_multi_episode_tick() {
    // One ProfilingPass reporting the same stage over three episodes in a single tick sums
    // into one accumulated stage profile (deterministic, count-only accumulation).
    let store = in_memory();
    insert_episode(&store, "one", 1);
    insert_episode(&store, "two", 2);
    insert_episode(&store, "three", 3);

    let mut consolidator = Consolidator::with_clock(store.clone(), config(), fixed_clock());
    consolidator.register(Box::new(ProfilingPass {
        stages: vec![StageProfile::enabled(STAGE_DETECTION, 2, 1, 0, 1, 0)],
    }));

    let report = consolidator.tick_once().await.expect("tick");
    assert_eq!(report.consolidated, 3);

    let detection = report
        .profile
        .stages()
        .iter()
        .find(|s| s.stage == STAGE_DETECTION)
        .copied()
        .expect("detection stage present");
    assert_eq!(
        detection.candidates_considered, 6,
        "2 candidates x 3 episodes accumulate"
    );
    assert_eq!(detection.derived, 3, "1 derived x 3 episodes accumulate");
    assert_eq!(
        detection.quarantined, 3,
        "1 quarantined x 3 episodes accumulate"
    );
}

#[tokio::test]
async fn a_failed_episode_contributes_no_profile_counts() {
    // Only a committed episode's passes ran to completion; a transiently-failed episode must
    // not leak partial counts into the tick profile.
    let store = in_memory();
    insert_episode(&store, "doomed", 1);
    let mut consolidator = Consolidator::with_clock(store.clone(), config(), fixed_clock());
    consolidator.register(Box::new(AlwaysFailPass { fatal: false }));

    let report = consolidator.tick_once().await.expect("tick");
    assert_eq!(report.retried, 1);
    assert!(
        report.profile.is_empty(),
        "a failed episode leaves the tick profile empty"
    );
}

#[tokio::test]
async fn a_noop_only_tick_leaves_the_profile_empty() {
    // A pass that reports no stage (NoopPass) consolidates without populating the profile, so
    // an empty profile is a faithful "nothing profiled this tick" signal.
    let store = in_memory();
    insert_episode(&store, "one", 1);
    let mut consolidator = Consolidator::with_clock(store.clone(), config(), fixed_clock());
    consolidator.register(Box::new(NoopPass));

    let report = consolidator.tick_once().await.expect("tick");
    assert_eq!(report.consolidated, 1);
    assert!(
        report.profile.is_empty(),
        "an unprofiled pass leaves the tick profile empty"
    );
}

#[test]
fn rule_versions_are_independent_of_pass_registration_order() {
    // The cursor's rule_versions map and the scheduler's content-derived actor id hash the
    // Display of this object, so it must serialize identically no matter what order passes
    // were registered in. That holds only while serde_json keeps object keys canonically
    // ordered — i.e. while the `preserve_order` feature stays OFF (see the canary below).
    let counter = Arc::new(AtomicUsize::new(0));
    let mut forward = Consolidator::with_clock(in_memory(), config(), fixed_clock());
    forward.register(Box::new(CountingPass {
        applied: Arc::clone(&counter),
    }));
    forward.register(Box::new(FlakyPass {
        applied: Arc::clone(&counter),
        fail_times: 0,
    }));

    let mut reversed = Consolidator::with_clock(in_memory(), config(), fixed_clock());
    reversed.register(Box::new(FlakyPass {
        applied: Arc::clone(&counter),
        fail_times: 0,
    }));
    reversed.register(Box::new(CountingPass {
        applied: Arc::clone(&counter),
    }));

    assert_eq!(forward.rule_versions(), reversed.rule_versions());
    assert_eq!(
        forward.rule_versions().to_string(),
        reversed.rule_versions().to_string()
    );
}

#[test]
fn serde_json_object_keys_serialize_in_canonical_order() {
    // Insurance for every content-derived id that hashes a serde_json object's Display
    // (audit payloads in the store, rule_versions / actor id here). serde_json sorts object
    // keys while the `preserve_order` feature is OFF; this canary fails loudly if a
    // dependency turns it on, which would silently break id stability across runs.
    let forward = serde_json::json!({ "alpha": 1, "beta": 2, "gamma": 3 });
    let shuffled = serde_json::json!({ "gamma": 3, "alpha": 1, "beta": 2 });
    assert_eq!(forward.to_string(), shuffled.to_string());
    assert_eq!(forward.to_string(), r#"{"alpha":1,"beta":2,"gamma":3}"#);
}
