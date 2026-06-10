//! Physical space reclaim (05 §3, M5.T03) — the second half of the erasure durability
//! story.
//!
//! A hard purge removes a node from live state and every index in the same write, but
//! two physical residues remain until a dense rebuild: the dead row slots (cleared to
//! empty, still allocated) and the vector index's tombstoned entries (search-unreachable
//! immediately; the HNSW structure keeps the stale entry until a rebuild).
//! [`Store::compact`] performs that rebuild: it densifies the live graph — every dead
//! and hole row dropped, all derived state including the vector index rebuilt from the
//! compacted columns — while readers keep the old snapshot until the dense one
//! publishes, and writers serialize with it exactly like a commit. Compaction can never
//! lose data: it is a pure transform of the live graph.
//!
//! What compaction does **not** do: it writes no snapshot and truncates no WAL. On a
//! persistent store the pre-purge property values still sit in WAL archives until the
//! snapshot pipeline publishes and rotates — that residency boundary is reported
//! honestly by the erasure facade, never papered over here.
//!
//! [`Store::compaction_pressure`] is the cheap scheduling probe: row counters that let
//! a host decide whether a dense rebuild is worth running without performing one.

use crate::error::StoreError;
use crate::store::Store;

/// What one compaction pass reclaimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactReport {
    /// Node rows (dead and aborted-hole) dropped.
    pub reclaimed_nodes: u64,
    /// Edge rows (dead and aborted-hole) dropped.
    pub reclaimed_edges: u64,
}

/// Row-space pressure: how much a compaction pass would reclaim right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CompactionPressure {
    /// Allocated node rows, live and reclaimable together.
    pub allocated_nodes: u64,
    /// Alive node rows.
    pub live_nodes: u64,
    /// Dead node rows a compaction pass can reclaim.
    pub reclaimable_nodes: u64,
    /// Allocated edge rows, live and reclaimable together.
    pub allocated_edges: u64,
    /// Alive edge rows.
    pub live_edges: u64,
    /// Dead edge rows a compaction pass can reclaim.
    pub reclaimable_edges: u64,
}

impl Store {
    /// Densify the live graph: drop every dead row, rebuild all derived state — the
    /// vector index included, which physically evicts purge tombstones (05 §3,
    /// M5.T03). Safe to run on any cadence; a pass with nothing to reclaim is cheap
    /// and lossless.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the rebuild fails its consistency check; the live
    /// graph is untouched on failure.
    pub fn compact(&self) -> Result<CompactReport, StoreError> {
        let report = self.graph().compact()?;
        Ok(CompactReport {
            reclaimed_nodes: report.reclaimed_nodes,
            reclaimed_edges: report.reclaimed_edges,
        })
    }

    /// Row-space pressure right now — the cheap probe a maintenance cadence reads to
    /// decide whether [`Store::compact`] is worth running.
    #[must_use]
    pub fn compaction_pressure(&self) -> CompactionPressure {
        let stats = self.graph().compaction_stats();
        CompactionPressure {
            allocated_nodes: stats.allocated_nodes,
            live_nodes: stats.live_nodes,
            reclaimable_nodes: stats.reclaimable_nodes,
            allocated_edges: stats.allocated_edges,
            live_edges: stats.live_edges,
            reclaimable_edges: stats.reclaimable_edges,
        }
    }
}
