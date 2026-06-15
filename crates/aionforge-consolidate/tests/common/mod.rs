//! Shared scaffolding for the consolidation scheduler integration tests: a fixed clock,
//! store/episode builders, and a small library of controllable [`ConsolidationPass`]
//! implementations. Split out so the scheduler suites stay within the file-size cap.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aionforge_consolidate::{
    Clock, ConsolidationConfig, ConsolidationPass, PassContext, PassError, PassProfile, PassRun,
    StageProfile,
};
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::time::Timestamp;
use aionforge_store::{NodeId, Store, StoreConfig};
use async_trait::async_trait;

pub fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

/// A fixed clock so lag and stamped times are deterministic.
#[derive(Clone)]
pub struct FixedClock(pub Timestamp);

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0.clone()
    }
}

pub fn fixed_clock() -> FixedClock {
    FixedClock(ts("2026-06-06T12:00:00-05:00[America/Chicago]"))
}

pub fn config() -> ConsolidationConfig {
    ConsolidationConfig::default()
}

pub fn store_config() -> StoreConfig {
    StoreConfig {
        embedding_dimension: 4,
    }
}

pub fn in_memory() -> Arc<Store> {
    let store = Store::open_with_config(store_config()).expect("open store");
    store
        .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
        .expect("migrate");
    Arc::new(store)
}

pub fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aionforge-consolidate-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

pub fn stats() -> Stats {
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
pub fn insert_episode(store: &Store, content: &str, minute: u32) -> NodeId {
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

pub fn episode_state(store: &Store, node_id: NodeId) -> ConsolidationState {
    store
        .episode_by_node_id(node_id)
        .expect("read")
        .expect("present")
        .consolidation_state
}

pub fn pending(store: &Store) -> u64 {
    store.consolidation_lag().expect("lag").episodes_pending
}

// --- Test passes ------------------------------------------------------------------

/// Counts how many times `apply` runs.
pub struct CountingPass {
    pub applied: Arc<AtomicUsize>,
}

#[async_trait]
impl ConsolidationPass for CountingPass {
    fn name(&self) -> &'static str {
        "counting"
    }
    fn version(&self) -> u32 {
        1
    }
    async fn apply(&self, _cx: &PassContext<'_>) -> Result<PassRun, PassError> {
        self.applied.fetch_add(1, Ordering::SeqCst);
        Ok(PassRun::empty())
    }
}

/// Fails transiently for the first `fail_times` applies, then succeeds.
pub struct FlakyPass {
    pub applied: Arc<AtomicUsize>,
    pub fail_times: usize,
}

#[async_trait]
impl ConsolidationPass for FlakyPass {
    fn name(&self) -> &'static str {
        "flaky"
    }
    fn version(&self) -> u32 {
        1
    }
    async fn apply(&self, _cx: &PassContext<'_>) -> Result<PassRun, PassError> {
        let n = self.applied.fetch_add(1, Ordering::SeqCst) + 1;
        if n <= self.fail_times {
            Err(PassError::Transient(format!("attempt {n}")))
        } else {
            Ok(PassRun::empty())
        }
    }
}

/// Fails transiently only on the episode whose content matches.
pub struct FailOnContentPass {
    pub fail_content: String,
}

#[async_trait]
impl ConsolidationPass for FailOnContentPass {
    fn name(&self) -> &'static str {
        "fail-on-content"
    }
    fn version(&self) -> u32 {
        1
    }
    async fn apply(&self, cx: &PassContext<'_>) -> Result<PassRun, PassError> {
        if cx.episode.content == self.fail_content {
            Err(PassError::Transient("targeted failure".to_string()))
        } else {
            Ok(PassRun::empty())
        }
    }
}

/// Always fails; `fatal` chooses the classification.
pub struct AlwaysFailPass {
    pub fatal: bool,
}

#[async_trait]
impl ConsolidationPass for AlwaysFailPass {
    fn name(&self) -> &'static str {
        "always-fail"
    }
    fn version(&self) -> u32 {
        1
    }
    async fn apply(&self, _cx: &PassContext<'_>) -> Result<PassRun, PassError> {
        if self.fatal {
            Err(PassError::Fatal("permanent".to_string()))
        } else {
            Err(PassError::Transient("temporary".to_string()))
        }
    }
}

/// Records the episode's own `consolidation_state` as observed from inside `apply` — proving
/// the scheduler marks the episode `in_progress` before any pass runs.
pub struct ObserveStatePass {
    pub observed: Arc<Mutex<Option<ConsolidationState>>>,
}

#[async_trait]
impl ConsolidationPass for ObserveStatePass {
    fn name(&self) -> &'static str {
        "observe-state"
    }
    fn version(&self) -> u32 {
        1
    }
    async fn apply(&self, cx: &PassContext<'_>) -> Result<PassRun, PassError> {
        let state = cx
            .store
            .episode_by_node_id(cx.episode_node_id)
            .expect("read episode")
            .expect("episode present")
            .consolidation_state;
        *self.observed.lock().expect("observed mutex") = Some(state);
        Ok(PassRun::empty())
    }
}

/// Records the episode's state from inside `apply`, then fails (transiently or fatally) — so a
/// test can prove the episode was `in_progress` before a failing pass settled it.
pub struct ObserveThenFailPass {
    pub observed: Arc<Mutex<Option<ConsolidationState>>>,
    pub fatal: bool,
}

#[async_trait]
impl ConsolidationPass for ObserveThenFailPass {
    fn name(&self) -> &'static str {
        "observe-then-fail"
    }
    fn version(&self) -> u32 {
        1
    }
    async fn apply(&self, cx: &PassContext<'_>) -> Result<PassRun, PassError> {
        let state = cx
            .store
            .episode_by_node_id(cx.episode_node_id)
            .expect("read episode")
            .expect("episode present")
            .consolidation_state;
        *self.observed.lock().expect("observed mutex") = Some(state);
        if self.fatal {
            Err(PassError::Fatal("permanent".to_string()))
        } else {
            Err(PassError::Transient("temporary".to_string()))
        }
    }
}

/// Tracks the maximum number of concurrent `apply` calls observed.
pub struct ConcurrencyProbePass {
    pub in_flight: Arc<AtomicUsize>,
    pub max_in_flight: Arc<AtomicUsize>,
}

#[async_trait]
impl ConsolidationPass for ConcurrencyProbePass {
    fn name(&self) -> &'static str {
        "probe"
    }
    fn version(&self) -> u32 {
        1
    }
    async fn apply(&self, _cx: &PassContext<'_>) -> Result<PassRun, PassError> {
        let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(current, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(5)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(PassRun::empty())
    }
}

/// Returns a fixed per-stage profile every apply, so a scheduler test can prove the tick
/// accumulates and exposes the profile (counts only). Derives nothing — the artifacts stay
/// empty, so it stands in for a real pass without needing a store-backed extractor.
pub struct ProfilingPass {
    pub stages: Vec<StageProfile>,
}

#[async_trait]
impl ConsolidationPass for ProfilingPass {
    fn name(&self) -> &'static str {
        "profiling"
    }
    fn version(&self) -> u32 {
        1
    }
    async fn apply(&self, _cx: &PassContext<'_>) -> Result<PassRun, PassError> {
        Ok(PassRun {
            output: Default::default(),
            profile: PassProfile::from_stages(self.stages.clone()),
        })
    }
}
