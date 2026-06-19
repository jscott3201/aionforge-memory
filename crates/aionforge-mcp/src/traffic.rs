//! Process-global in/out traffic accounting plus a periodic heartbeat log
//! (logging-foundation, task #9 PR1).
//!
//! Two atomic counters tally the bytes the memory server **receives** as capture content (IN)
//! and **serves back** as recall responses (OUT). A background task emits a periodic `tracing`
//! line carrying the cumulative totals and the per-interval delta, so an operator watching the
//! logs sees how much memory is flowing through the server over time without scraping metrics.
//!
//! Bytes are authoritative; tokens are a clearly-labeled *estimate* from a coarse divisor (the
//! server cannot run the calling client's tokenizer — same caveat as [`crate::telemetry`]).
//!
//! Coverage is the MCP tool boundary: IN counts the `content` of `capture` / `batch_capture`
//! (the memory text clients push to be stored); OUT counts the rendered recall responses of
//! `search` / `read_memory` / `session_manifest` / the `work_*` readers (the dominant outbound
//! payload). Tiny control traffic (query params, receipts) is intentionally not counted — this
//! is a memory-throughput signal, not a wire-level byte meter.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Coarse chars-per-token divisor for the labeled token estimates. Never exact — a faithful
/// count needs the client's own tokenizer.
pub(crate) const TOKEN_ESTIMATE_BYTES_PER_TOKEN: u64 = 4;

/// Default heartbeat cadence when the operator sets no override: every 5 minutes.
pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(300);

static BYTES_IN: AtomicU64 = AtomicU64::new(0);
static BYTES_OUT: AtomicU64 = AtomicU64::new(0);

/// Process-global memory traffic totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TrafficSnapshot {
    /// Cumulative bytes accepted as memory content.
    pub(crate) bytes_in_total: u64,
    /// Cumulative bytes served back in memory-bearing responses.
    pub(crate) bytes_out_total: u64,
}

impl TrafficSnapshot {
    /// Cumulative inbound token estimate from the documented byte divisor.
    #[must_use]
    pub(crate) fn estimated_tokens_in_total(self) -> u64 {
        self.bytes_in_total / TOKEN_ESTIMATE_BYTES_PER_TOKEN
    }

    /// Cumulative outbound token estimate from the documented byte divisor.
    #[must_use]
    pub(crate) fn estimated_tokens_out_total(self) -> u64 {
        self.bytes_out_total / TOKEN_ESTIMATE_BYTES_PER_TOKEN
    }
}

/// Record `bytes` of capture content received from a client (IN). `Relaxed` ordering is
/// sufficient — these are independent running totals, never used to guard other state.
pub(crate) fn record_in(bytes: u64) {
    BYTES_IN.fetch_add(bytes, Ordering::Relaxed);
}

/// Record `bytes` of recall response served to a client (OUT).
pub(crate) fn record_out(bytes: u64) {
    BYTES_OUT.fetch_add(bytes, Ordering::Relaxed);
}

/// The current cumulative byte totals.
pub(crate) fn snapshot() -> TrafficSnapshot {
    TrafficSnapshot {
        bytes_in_total: BYTES_IN.load(Ordering::Relaxed),
        bytes_out_total: BYTES_OUT.load(Ordering::Relaxed),
    }
}

/// Emit one structured traffic line at `info`. `phase` says why it fired (`heartbeat` for the
/// periodic tick, `shutdown` for the final summary). All fields are integers — no content.
fn emit(phase: &'static str, in_total: u64, out_total: u64, in_delta: u64, out_delta: u64) {
    tracing::info!(
        target: "aionforge::traffic",
        phase,
        bytes_in_total = in_total,
        bytes_out_total = out_total,
        bytes_in_delta = in_delta,
        bytes_out_delta = out_delta,
        est_tokens_in_total = in_total / TOKEN_ESTIMATE_BYTES_PER_TOKEN,
        est_tokens_out_total = out_total / TOKEN_ESTIMATE_BYTES_PER_TOKEN,
        est_tokens_in_delta = in_delta / TOKEN_ESTIMATE_BYTES_PER_TOKEN,
        est_tokens_out_delta = out_delta / TOKEN_ESTIMATE_BYTES_PER_TOKEN,
        "memory traffic",
    );
}

/// Log the cumulative totals once (e.g. on graceful shutdown), with deltas equal to totals so
/// the line reads as a session summary.
pub fn log_totals(phase: &'static str) {
    let totals = snapshot();
    emit(
        phase,
        totals.bytes_in_total,
        totals.bytes_out_total,
        totals.bytes_in_total,
        totals.bytes_out_total,
    );
}

/// Run the periodic traffic heartbeat until the task is dropped (i.e. for the server's life).
///
/// Each tick logs the cumulative totals and the delta since the previous tick. The first
/// immediate tick from [`tokio::time::interval`] is consumed so the first *logged* line lands
/// one full interval in (an immediate all-zero line at boot would be noise). A zero interval
/// disables the heartbeat (returns immediately); callers should simply not spawn it then, but
/// this guard makes the contract total.
pub async fn run_heartbeat(interval: Duration) {
    if interval.is_zero() {
        return;
    }
    let mut ticker = tokio::time::interval(interval);
    // If a tick is missed (e.g. a stalled executor), skip the backlog rather than firing a
    // burst of catch-up lines — one summary per real interval is the intent.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await; // consume the immediate first tick
    let mut last = snapshot();
    loop {
        ticker.tick().await;
        let totals = snapshot();
        emit(
            "heartbeat",
            totals.bytes_in_total,
            totals.bytes_out_total,
            totals.bytes_in_total.saturating_sub(last.bytes_in_total),
            totals.bytes_out_total.saturating_sub(last.bytes_out_total),
        );
        last = totals;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_in_and_out_accumulate_into_the_snapshot() {
        // These atomics are process-global and other tests can legitimately record traffic at
        // the same time, so assert the minimum local contribution rather than an exact delta.
        let before = snapshot();
        record_in(100);
        record_in(40);
        record_out(2048);
        let after = snapshot();
        assert!(
            after.bytes_in_total >= before.bytes_in_total + 140,
            "IN accumulates every record"
        );
        assert!(
            after.bytes_out_total >= before.bytes_out_total + 2048,
            "OUT accumulates every record"
        );
    }

    #[tokio::test]
    async fn a_zero_interval_heartbeat_returns_immediately() {
        // The disable contract: a zero interval must not spin or block.
        run_heartbeat(Duration::ZERO).await;
    }
}
