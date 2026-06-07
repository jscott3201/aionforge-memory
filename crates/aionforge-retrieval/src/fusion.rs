//! Reciprocal Rank Fusion over signal rankings (03 §2).
//!
//! BM25 scores and cosine distances are not comparable, so signals are fused by
//! rank, not score: each candidate's fused score is the weighted sum of `1 / (k +
//! rank)` across the signals that ranked it (the validated low-tuning default, with
//! `k = 60`). Per-mode weights select intent; a weight of zero elides a signal
//! entirely.
//!
//! Fusion is deterministic, which is a hard requirement (03 §6): identical inputs and
//! graph state yield identical output, and any permutation of the input rankings
//! yields byte-identical output. Two things make that hold despite floating-point
//! addition not being associative — each candidate's contributions are summed in a
//! fixed order (by [`Signal`]), and the final order breaks ties by node id. The
//! serialization-id tie-break the rendered text needs (03 §6) is applied later, when
//! the recall bundle renders; within a fixed graph state the node-id tie-break here
//! is already deterministic and permutation-independent.

use std::collections::HashMap;

use aionforge_store::NodeId;

use crate::signals::{Signal, SignalRanking};

/// The default RRF constant — the rank-fusion smoothing term (03 §2).
pub const DEFAULT_RRF_K: f64 = 60.0;

/// A signal ranking paired with the weight its retrieval mode assigns it (03 §2).
#[derive(Debug, Clone, PartialEq)]
pub struct WeightedRanking {
    /// The mode weight, expected non-negative. Zero elides the signal from fusion
    /// entirely; a negative weight has no rank-fusion meaning (there is no
    /// anti-ranking) and is a caller error.
    pub weight: f64,
    /// The signal's ranked candidate list.
    pub ranking: SignalRanking,
}

impl WeightedRanking {
    /// Pair a ranking with its mode weight.
    #[must_use]
    pub fn new(weight: f64, ranking: SignalRanking) -> Self {
        Self { weight, ranking }
    }
}

/// One signal's contribution to a fused candidate, kept for the retrieval
/// explanation (03 §6).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Contribution {
    /// The contributing signal.
    pub signal: Signal,
    /// The candidate's rank in that signal's list (0-based, best-first).
    pub rank: usize,
    /// The mode weight applied to that signal.
    pub weight: f64,
}

impl Contribution {
    /// This contribution's term in the RRF sum: `weight / (k + rank + 1)`. The `+ 1`
    /// turns the 0-based rank into the 1-based rank the RRF constant was tuned for, so
    /// the top of a list contributes `weight / (k + 1)`.
    fn term(&self, k_const: f64) -> f64 {
        self.weight / (k_const + self.rank as f64 + 1.0)
    }
}

/// A candidate after fusion: its summed score and which signals ranked it.
#[derive(Debug, Clone, PartialEq)]
pub struct FusedCandidate {
    /// The candidate node.
    pub node: NodeId,
    /// The fused RRF score (higher is better).
    pub score: f64,
    /// The per-signal contributions, in canonical [`Signal`] order.
    pub contributions: Vec<Contribution>,
}

/// Fuse weighted signal rankings into one ranked list by Reciprocal Rank Fusion.
///
/// The result is ordered by fused score descending, ties broken by node id ascending.
/// Zero-weight rankings are skipped, so a signal a mode switches off contributes
/// nothing and leaves no trace in the contributions.
///
/// Callers pass non-negative weights and a positive `k_const` (the tuned default is
/// [`DEFAULT_RRF_K`]); those are the only inputs RRF is defined for. A negative
/// weight or a non-positive `k_const` is a caller error — checked with
/// `debug_assert!`, since the only caller is the in-process router. The output stays
/// deterministic regardless (the ordering relies on a total order over scores, not on
/// their sign), it just stops carrying rank-fusion meaning.
#[must_use]
pub fn fuse(rankings: &[WeightedRanking], k_const: f64) -> Vec<FusedCandidate> {
    debug_assert!(k_const > 0.0, "RRF k_const must be positive");
    let mut by_node: HashMap<NodeId, Vec<Contribution>> = HashMap::new();
    for weighted in rankings {
        debug_assert!(
            weighted.weight >= 0.0,
            "a signal weight must be non-negative"
        );
        if weighted.weight == 0.0 {
            continue; // a zero weight elides the signal (03 §2)
        }
        for candidate in &weighted.ranking.candidates {
            by_node
                .entry(candidate.node)
                .or_default()
                .push(Contribution {
                    signal: weighted.ranking.signal,
                    rank: candidate.rank,
                    weight: weighted.weight,
                });
        }
    }

    let mut fused: Vec<FusedCandidate> = by_node
        .into_iter()
        .map(|(node, mut contributions)| {
            // Sum in a fixed order (by signal) so the float result does not depend on
            // the order the rankings were supplied.
            contributions.sort_by_key(|contribution| contribution.signal);
            let score = contributions
                .iter()
                .map(|contribution| contribution.term(k_const))
                .sum();
            FusedCandidate {
                node,
                score,
                contributions,
            }
        })
        .collect();

    // Score descending; node id ascending breaks ties deterministically. `total_cmp`
    // is a total order over every f64 (including any non-finite value), so the sort
    // is well-defined and reproducible whatever the scores turn out to be.
    fused.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
    fused
}
