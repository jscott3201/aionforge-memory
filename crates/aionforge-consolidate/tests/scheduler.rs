//! Acceptance tests for the consolidation scheduler (M2.T03, write-and-consolidation
//! §2–§3): the cursor persists and resumes, concurrency is bounded, lag is observable,
//! and a crash mid-pass resumes from the last position and never double-applies.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aionforge_consolidate::{
    Clock, ConsolidationConfig, ConsolidationPass, Consolidator, NoopPass, PassContext, PassError,
    PassOutput,
};
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::time::Timestamp;
use aionforge_store::{NodeId, Store, StoreConfig};
use async_trait::async_trait;

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

/// A fixed clock so lag and stamped times are deterministic.
#[derive(Clone)]
struct FixedClock(Timestamp);

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0.clone()
    }
}

fn fixed_clock() -> FixedClock {
    FixedClock(ts("2026-06-06T12:00:00-05:00[America/Chicago]"))
}

fn config() -> ConsolidationConfig {
    ConsolidationConfig::default()
}

fn store_config() -> StoreConfig {
    StoreConfig {
        embedding_dimension: 4,
    }
}

fn in_memory() -> Arc<Store> {
    let store = Store::open_with_config(store_config()).expect("open store");
    store
        .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
        .expect("migrate");
    Arc::new(store)
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aionforge-consolidate-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
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

/// Insert a `raw` episode; minute `n` sets a distinct, ordered ingested/captured time.
fn insert_episode(store: &Store, content: &str, minute: u32) -> NodeId {
    let stamp = format!("2026-06-06T09:{minute:02}:00-05:00[America/Chicago]");
    let episode = Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts(&stamp),
            namespace: Namespace::Agent("alice".to_string()),
            expired_at: None,
        },
        stats: stats(),
        content: content.to_string(),
        role: Role::User,
        captured_at: ts(&stamp),
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(format!("{content}-{minute}").as_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("insert episode")
}

fn episode_state(store: &Store, node_id: NodeId) -> ConsolidationState {
    store
        .episode_by_node_id(node_id)
        .expect("read")
        .expect("present")
        .consolidation_state
}

fn pending(store: &Store) -> u64 {
    store.consolidation_lag().expect("lag").episodes_pending
}

// --- Test passes ------------------------------------------------------------------

/// Counts how many times `apply` runs.
struct CountingPass {
    applied: Arc<AtomicUsize>,
}

#[async_trait]
impl ConsolidationPass for CountingPass {
    fn name(&self) -> &'static str {
        "counting"
    }
    fn version(&self) -> u32 {
        1
    }
    async fn apply(&self, _cx: &PassContext<'_>) -> Result<PassOutput, PassError> {
        self.applied.fetch_add(1, Ordering::SeqCst);
        Ok(PassOutput::default())
    }
}

/// Fails transiently for the first `fail_times` applies, then succeeds.
struct FlakyPass {
    applied: Arc<AtomicUsize>,
    fail_times: usize,
}

#[async_trait]
impl ConsolidationPass for FlakyPass {
    fn name(&self) -> &'static str {
        "flaky"
    }
    fn version(&self) -> u32 {
        1
    }
    async fn apply(&self, _cx: &PassContext<'_>) -> Result<PassOutput, PassError> {
        let n = self.applied.fetch_add(1, Ordering::SeqCst) + 1;
        if n <= self.fail_times {
            Err(PassError::Transient(format!("attempt {n}")))
        } else {
            Ok(PassOutput::default())
        }
    }
}

/// Fails transiently only on the episode whose content matches.
struct FailOnContentPass {
    fail_content: String,
}

#[async_trait]
impl ConsolidationPass for FailOnContentPass {
    fn name(&self) -> &'static str {
        "fail-on-content"
    }
    fn version(&self) -> u32 {
        1
    }
    async fn apply(&self, cx: &PassContext<'_>) -> Result<PassOutput, PassError> {
        if cx.episode.content == self.fail_content {
            Err(PassError::Transient("targeted failure".to_string()))
        } else {
            Ok(PassOutput::default())
        }
    }
}

/// Always fails; `fatal` chooses the classification.
struct AlwaysFailPass {
    fatal: bool,
}

#[async_trait]
impl ConsolidationPass for AlwaysFailPass {
    fn name(&self) -> &'static str {
        "always-fail"
    }
    fn version(&self) -> u32 {
        1
    }
    async fn apply(&self, _cx: &PassContext<'_>) -> Result<PassOutput, PassError> {
        if self.fatal {
            Err(PassError::Fatal("permanent".to_string()))
        } else {
            Err(PassError::Transient("temporary".to_string()))
        }
    }
}

/// Tracks the maximum number of concurrent `apply` calls observed.
struct ConcurrencyProbePass {
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
}

#[async_trait]
impl ConsolidationPass for ConcurrencyProbePass {
    fn name(&self) -> &'static str {
        "probe"
    }
    fn version(&self) -> u32 {
        1
    }
    async fn apply(&self, _cx: &PassContext<'_>) -> Result<PassOutput, PassError> {
        let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(current, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(5)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(PassOutput::default())
    }
}

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
    let store = Arc::new(Store::recover(&dir, store_config()).expect("recover"));
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
