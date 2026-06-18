//! The serializable report of a `min_relevance` floor sweep.
//!
//! A sweep recalls the same labeled query set at each candidate floor value and records,
//! per floor, the off-topic-rejection rate alongside the cost (false-rejection) and
//! quality (recall@k / nDCG@k) it pays for that rejection. The report is the artifact a
//! steward reads to pick a responsible per-class floor: the highest rejection that keeps
//! false-rejection at (or near) zero.

use std::io::{self, Write};

use serde::{Deserialize, Serialize};

/// One row of a floor sweep: the metrics observed at a single `min_relevance` value.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FloorReport {
    /// The `min_relevance` floor this row was measured at.
    pub floor: f64,
    /// Fraction of off-topic (negative) queries correctly rejected (higher is better).
    pub rejection_rate: f64,
    /// Fraction of on-topic (positive) queries whose gold was wrongly dropped (lower is
    /// better) — the cost of the floor.
    pub false_rejection_rate: f64,
    /// Mean recall@k over the positive queries at this floor.
    pub recall_at_k: f64,
    /// Mean nDCG@k over the positive queries at this floor.
    pub ndcg_at_k: f64,
}

/// A whole floor sweep: the `k` used for the ranking metrics and one row per floor value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepReport {
    /// The cutoff `k` used for recall@k / nDCG@k across every row.
    pub k: usize,
    /// One row per swept floor value, in sweep order.
    pub rows: Vec<FloorReport>,
}

impl SweepReport {
    /// A sweep report over the given `k` and rows.
    #[must_use]
    pub fn new(k: usize, rows: Vec<FloorReport>) -> Self {
        Self { k, rows }
    }

    /// The best floor: the highest `rejection_rate` whose `false_rejection_rate` does not
    /// exceed `max_false_rejection`, breaking ties toward the *lower* floor (the least
    /// aggressive setting that achieves the rejection). `None` if no row qualifies.
    #[must_use]
    pub fn best_floor(&self, max_false_rejection: f64) -> Option<&FloorReport> {
        self.rows
            .iter()
            .filter(|row| row.false_rejection_rate <= max_false_rejection)
            .max_by(|a, b| {
                a.rejection_rate
                    .total_cmp(&b.rejection_rate)
                    .then(b.floor.total_cmp(&a.floor))
            })
    }

    /// Write the report as pretty JSON to `writer`.
    ///
    /// # Errors
    /// Propagates any I/O or serialization error from the writer.
    pub fn write_json<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        serde_json::to_writer_pretty(&mut *writer, self)?;
        writer.write_all(b"\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(floor: f64, rejection: f64, false_rejection: f64) -> FloorReport {
        FloorReport {
            floor,
            rejection_rate: rejection,
            false_rejection_rate: false_rejection,
            recall_at_k: 1.0 - false_rejection,
            ndcg_at_k: 1.0 - false_rejection,
        }
    }

    #[test]
    fn best_floor_maximizes_rejection_under_the_false_rejection_cap() {
        let report = SweepReport::new(
            5,
            vec![
                row(0.0, 0.0, 0.0),  // rejects nothing
                row(0.5, 0.8, 0.0),  // strong rejection, no cost — the winner
                row(0.7, 1.0, 0.25), // perfect rejection but over the cost cap
            ],
        );
        let best = report.best_floor(0.05).expect("a row qualifies");
        assert!((best.floor - 0.5).abs() < 1e-12, "0.5 wins under the cap");
    }

    #[test]
    fn best_floor_is_none_when_every_row_exceeds_the_cap() {
        let report = SweepReport::new(5, vec![row(0.7, 1.0, 0.5)]);
        assert!(report.best_floor(0.05).is_none());
    }

    #[test]
    fn write_json_round_trips() {
        let report = SweepReport::new(5, vec![row(0.5, 0.8, 0.0)]);
        let mut buf = Vec::new();
        report.write_json(&mut buf).expect("write");
        let parsed: SweepReport = serde_json::from_slice(&buf).expect("parse");
        assert_eq!(parsed, report);
    }
}
