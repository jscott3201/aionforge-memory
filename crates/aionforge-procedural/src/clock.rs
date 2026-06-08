//! The injected clock for procedural memory's own bookkeeping timestamps.
//!
//! Saving a skill stamps the new version's `ingested_at` and its audit `occurred_at`; recording
//! an outcome stamps `last_success_at` / `last_failure_at`. Those are the substrate's own "when
//! did this happen" times — legitimately *now* — but they are injected through this seam rather
//! than read from an ambient clock, so tests are deterministic and stored time is never a guess
//! (the same discipline the consolidator uses for its bookkeeping times).

use aionforge_domain::time::Timestamp;

/// A source of the current time for procedural memory's bookkeeping.
pub trait Clock: Send + Sync + 'static {
    /// The current instant.
    fn now(&self) -> Timestamp;
}

/// The production clock: the system zoned time.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}
