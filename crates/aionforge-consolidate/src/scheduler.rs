//! The background consolidator: discover work, apply passes, flip-and-advance, repeat
//! (write-and-consolidation §2–§3).
//!
//! [`Consolidator::tick_once`] is the unit of work and the deterministic test seam: it
//! drains one bounded batch of pending episodes, runs the registered passes over each,
//! and — on success — flips the episode and advances the cursor in a single atomic
//! commit. [`Consolidator::start`] spawns a task that calls `tick_once` on a timer until
//! shut down.
//!
//! Episodes are processed one at a time in commit order, and a tick **stops at the first
//! episode that does not consolidate**, so the cursor advances only over the contiguous
//! consolidated prefix and never past a failure. A transient failure leaves the episode
//! `raw`, so it is the oldest pending next tick and the cursor genuinely holds at it
//! until it succeeds or escalates; a fatal failure marks the episode `failed` (retained
//! and audited, excluded from the queue), so later ticks proceed past it — the failed
//! episode awaits an operator reconcile/skip rather than wedging the whole pipeline. A
//! crash can lose at most the in-flight episode, which stays `raw` and is re-run, never
//! double-committed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aionforge_domain::blocks::Identity;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::Timestamp;
use aionforge_store::{ConsolidationCursor, ConsolidationWorkItem, Store};

use crate::clock::{Clock, SystemClock};
use crate::config::ConsolidationConfig;
use crate::error::ConsolidationError;
use crate::lag::ConsolidationLag;
use crate::pass::{ConsolidationPass, PassContext, PassError};

/// What one tick accomplished.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TickReport {
    /// Episodes consolidated this tick.
    pub consolidated: usize,
    /// Episodes left `raw` after a transient failure (to retry next tick).
    pub retried: usize,
    /// Episodes marked `failed` this tick.
    pub failed: usize,
    /// Episodes still pending after the tick (the live backlog).
    pub pending_after: u64,
}

/// The outcome of consolidating a single episode.
enum EpisodeOutcome {
    Consolidated,
    Retried,
    Failed,
}

/// The asynchronous consolidator over a shared store.
///
/// Holds the registered passes, the tuning, an injected clock, and the in-memory retry
/// accounting. Generic over the [`Clock`] so tests inject a fixed time; production uses
/// [`SystemClock`].
pub struct Consolidator<C: Clock = SystemClock> {
    store: Arc<Store>,
    passes: Vec<Box<dyn ConsolidationPass>>,
    config: ConsolidationConfig,
    clock: C,
    /// The substrate actor id stamped on this consolidator's audit events.
    actor_id: Id,
    /// Per-episode transient-failure counts, keyed by episode id string.
    attempts: Mutex<HashMap<String, u32>>,
}

impl Consolidator<SystemClock> {
    /// Build a consolidator with the production system clock.
    #[must_use]
    pub fn new(store: Arc<Store>, config: ConsolidationConfig) -> Self {
        Self::with_clock(store, config, SystemClock)
    }
}

impl<C: Clock> Consolidator<C> {
    /// Build a consolidator with an explicit clock (the test seam).
    #[must_use]
    pub fn with_clock(store: Arc<Store>, config: ConsolidationConfig, clock: C) -> Self {
        Self {
            store,
            passes: Vec::new(),
            config,
            clock,
            actor_id: Id::generate(),
            attempts: Mutex::new(HashMap::new()),
        }
    }

    /// Register a pass. Passes run in registration order over each episode.
    pub fn register(&mut self, pass: Box<dyn ConsolidationPass>) {
        self.passes.push(pass);
    }

    /// The `{pass_name: version}` map of the enabled passes, for the cursor.
    #[must_use]
    pub fn rule_versions(&self) -> serde_json::Value {
        let map: serde_json::Map<String, serde_json::Value> = self
            .passes
            .iter()
            .filter(|pass| pass.enabled())
            .map(|pass| (pass.name().to_owned(), serde_json::json!(pass.version())))
            .collect();
        serde_json::Value::Object(map)
    }

    /// Drain one bounded batch of pending episodes (the unit of work; the test seam).
    ///
    /// # Errors
    /// Returns [`ConsolidationError`] if a store read or a flip the scheduler issues
    /// fails. A *pass* failure is not an error here — it is audited and reflected in the
    /// returned [`TickReport`].
    pub async fn tick_once(&self) -> Result<TickReport, ConsolidationError> {
        let batch = self
            .store
            .discover_consolidation_work(self.config.batch_size)?;
        let mut report = TickReport::default();
        for item in batch {
            // Stop at the first episode that does not consolidate: a later episode's
            // commit must never advance the cursor past a held-back failure (the cursor
            // tracks the contiguous consolidated prefix). The skipped tail stays pending
            // and is rediscovered, in order, next tick.
            match self.process_episode(&item).await? {
                EpisodeOutcome::Consolidated => report.consolidated += 1,
                EpisodeOutcome::Retried => {
                    report.retried += 1;
                    break;
                }
                EpisodeOutcome::Failed => {
                    report.failed += 1;
                    break;
                }
            }
        }
        let lag = self.lag()?;
        emit_lag_metrics(&lag, self.config.lag_ceiling);
        report.pending_after = lag.episodes_pending;
        Ok(report)
    }

    /// The current consolidation lag, against this consolidator's clock.
    ///
    /// # Errors
    /// Returns [`ConsolidationError`] if the backlog query fails.
    pub fn lag(&self) -> Result<ConsolidationLag, ConsolidationError> {
        let snapshot = self.store.consolidation_lag()?;
        Ok(ConsolidationLag::from_snapshot(
            &snapshot,
            &self.clock.now(),
        ))
    }

    /// Spawn the background loop, returning a handle that can shut it down.
    ///
    /// Runs the crash-recovery reset first (an episode left `in_progress` by an
    /// interrupted prior run is returned to `raw` so the next pass re-runs it cleanly),
    /// then calls [`Self::tick_once`] every `tick_interval` until the handle signals
    /// shutdown. A reset or tick error is logged, not fatal — the next tick retries.
    #[must_use]
    pub fn start(self) -> ConsolidationHandle {
        let consolidator = Arc::new(self);
        match consolidator.store.reset_in_progress_episodes() {
            Ok(0) => {}
            Ok(count) => {
                tracing::info!(
                    count,
                    "reset in_progress episodes left by an interrupted run"
                )
            }
            Err(error) => {
                tracing::error!(%error, "failed to reset in_progress episodes at startup")
            }
        }
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(consolidator.config.tick_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Err(error) = consolidator.tick_once().await {
                            tracing::error!(%error, "consolidation tick failed");
                        }
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        });
        ConsolidationHandle {
            shutdown: shutdown_tx,
            task,
        }
    }

    /// Run the passes over one episode and commit the outcome.
    async fn process_episode(
        &self,
        item: &ConsolidationWorkItem,
    ) -> Result<EpisodeOutcome, ConsolidationError> {
        let now = self.clock.now();
        let rule_versions = self.rule_versions();
        for pass in self.passes.iter().filter(|pass| pass.enabled()) {
            let cx = PassContext {
                store: &self.store,
                episode_node_id: item.node_id,
                episode: &item.episode,
                now: now.clone(),
                rule_versions: &rule_versions,
            };
            let result =
                match tokio::time::timeout(self.config.apply_timeout, pass.apply(&cx)).await {
                    Ok(result) => result,
                    Err(_elapsed) => Err(PassError::Transient(format!(
                        "pass `{}` exceeded its {:?} timeout",
                        pass.name(),
                        self.config.apply_timeout
                    ))),
                };
            if let Err(error) = result {
                return self.handle_failure(item, pass.name(), &error, &now);
            }
            // M2.T03: a successful pass derives nothing to materialize; the flip below is
            // the whole of the committed progress.
        }

        // Every pass succeeded: flip the episode and advance the cursor atomically. The
        // expected state is the episode's current state (raw, or in_progress after a
        // crash-recovery reset), so the guard accepts exactly the row we discovered.
        let cursor = ConsolidationCursor {
            last_position: ConsolidationCursor::watermark_for(&item.episode),
            last_episode_id: Some(item.episode.identity.id.clone()),
            last_processed_at: Some(now.clone()),
            rule_versions,
        };
        self.store.commit_consolidation_episode(
            item.node_id,
            item.episode.consolidation_state,
            ConsolidationState::Consolidated,
            &cursor,
            &now,
        )?;
        self.forget_attempts(item.episode.identity.id.as_str());
        Ok(EpisodeOutcome::Consolidated)
    }

    /// Audit a pass failure and decide retry vs. fatal.
    fn handle_failure(
        &self,
        item: &ConsolidationWorkItem,
        pass_name: &str,
        error: &PassError,
        now: &Timestamp,
    ) -> Result<EpisodeOutcome, ConsolidationError> {
        let key = item.episode.identity.id.as_str().to_owned();
        let attempts = {
            let mut guard = self.attempts.lock().expect("attempts mutex poisoned");
            let count = guard.entry(key).or_insert(0);
            *count += 1;
            *count
        };
        let fatal = matches!(error, PassError::Fatal(_)) || attempts > self.config.max_retries;
        let audit = self.failure_audit(&item.episode, pass_name, error, attempts, fatal, now);
        self.store
            .record_consolidation_failure(item.node_id, &audit, fatal)?;
        let episode_id = item.episode.identity.id.as_str();
        if fatal {
            self.forget_attempts(episode_id);
            tracing::error!(
                episode = episode_id,
                pass = pass_name,
                attempts,
                "consolidation pass failed fatally; episode marked failed"
            );
            Ok(EpisodeOutcome::Failed)
        } else {
            tracing::warn!(
                episode = episode_id,
                pass = pass_name,
                attempts,
                "consolidation pass failed transiently; will retry"
            );
            Ok(EpisodeOutcome::Retried)
        }
    }

    /// Drop the retry count for an episode that left the pending set.
    fn forget_attempts(&self, episode_id: &str) {
        self.attempts
            .lock()
            .expect("attempts mutex poisoned")
            .remove(episode_id);
    }

    /// Build the `consolidation_failed` audit event (unsigned, like the capture path).
    fn failure_audit(
        &self,
        episode: &Episode,
        pass_name: &str,
        error: &PassError,
        attempts: u32,
        fatal: bool,
        now: &Timestamp,
    ) -> AuditEvent {
        AuditEvent {
            identity: Identity {
                id: Id::generate(),
                ingested_at: now.clone(),
                namespace: Namespace::System,
                expired_at: None,
            },
            kind: AuditKind::ConsolidationFailed,
            subject_id: episode.identity.id.clone(),
            actor_id: self.actor_id.clone(),
            payload: serde_json::json!({
                "pass": pass_name,
                "reason": error.to_string(),
                "kind": if fatal { "fatal" } else { "transient" },
                "attempts": attempts,
            }),
            signature: String::new(),
            occurred_at: now.clone(),
        }
    }
}

/// Emit the lag gauges and warn when the ceiling is breached.
fn emit_lag_metrics(lag: &ConsolidationLag, ceiling: std::time::Duration) {
    metrics::gauge!("consolidation_lag_seconds").set(lag.oldest_pending_lag.as_secs_f64());
    metrics::gauge!("consolidation_episodes_pending").set(lag.episodes_pending as f64);
    metrics::gauge!("consolidation_episodes_failed").set(lag.episodes_failed as f64);
    if lag.oldest_pending_lag > ceiling {
        tracing::warn!(
            lag_seconds = lag.oldest_pending_lag.as_secs_f64(),
            pending = lag.episodes_pending,
            "consolidation lag exceeds the configured ceiling"
        );
    }
}

/// A handle to a spawned consolidation loop.
pub struct ConsolidationHandle {
    shutdown: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl ConsolidationHandle {
    /// Signal the loop to stop and await its exit (graceful: the in-flight tick finishes).
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }

    /// Abort the loop without waiting (for drop paths and tests).
    pub fn abort(self) {
        self.task.abort();
    }
}
