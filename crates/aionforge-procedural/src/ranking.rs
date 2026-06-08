//! The pure ranking math: Beta-posterior reliability and reciprocal rank fusion (05; M3.T04).
//!
//! Kept free of any store, embedder, or even the concrete node-id type so it is exhaustively
//! unit-testable on plain numbers. Both pieces are deterministic: identical inputs yield
//! identical outputs, independent of the order the signal rankings are supplied.

use std::collections::HashMap;
use std::hash::Hash;

/// The Beta-posterior mean of a success rate: `(α₀ + s) / (α₀ + β₀ + s + f)`.
///
/// A skill's observed successes `s` and failures `f` update a Beta prior; the posterior mean is
/// the reliability weight retrieval multiplies into the problem-match score. With the weak
/// Beta(1,1) prior a fresh `0/0` skill scores a neutral `0.5` (neither boosted nor buried), a
/// `1/0` skill `2/3` (not an over-trusted `1.0`), and evidence pulls the score toward the
/// empirical rate as it accumulates. This is the same Beta model the M4.T05 trust scoring will
/// share, so reliability and trust stay on one footing.
pub(crate) fn reliability(prior_alpha: f64, prior_beta: f64, success: u64, failure: u64) -> f64 {
    let s = success as f64;
    let f = failure as f64;
    (prior_alpha + s) / (prior_alpha + prior_beta + s + f)
}

/// One signal's ranked candidates (best-first keys) paired with its fusion weight.
pub(crate) struct WeightedRanking<'a, N> {
    /// The mode weight; zero elides the signal from fusion.
    pub weight: f64,
    /// The signal's ranked keys, best first.
    pub nodes: &'a [N],
}

/// Fuse weighted signal rankings by Reciprocal Rank Fusion, returning each key's fused score as
/// `(key, score)` sorted by score descending, ties broken by key ascending.
///
/// Mirrors the retrieval-path RRF (03 §2): a key's score is the weighted sum of
/// `weight / (k_const + rank + 1)` over the signals that ranked it (the `+ 1` makes it the
/// 1-based rank the constant was tuned for). BM25 scores and cosine distances are not
/// comparable, so fusing by rank — not magnitude — is what lets the two signals combine at all.
/// A zero-weight signal contributes nothing. `k_const` is expected positive (the caller's
/// config validates it); the result stays deterministic regardless.
pub(crate) fn rrf<N>(rankings: &[WeightedRanking<'_, N>], k_const: f64) -> Vec<(N, f64)>
where
    N: Copy + Eq + Hash + Ord,
{
    let mut scores: HashMap<N, f64> = HashMap::new();
    for ranking in rankings {
        if ranking.weight == 0.0 {
            continue;
        }
        for (rank, &node) in ranking.nodes.iter().enumerate() {
            let term = ranking.weight / (k_const + rank as f64 + 1.0);
            *scores.entry(node).or_insert(0.0) += term;
        }
    }
    let mut fused: Vec<(N, f64)> = scores.into_iter().collect();
    // Score descending; key ascending breaks ties. `total_cmp` is a total order over every f64,
    // so the sort is well-defined and reproducible whatever the scores turn out to be.
    fused.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    fused
}

/// Cosine similarity of two equal-length vectors, in `[-1, 1]`; `0.0` for a length
/// mismatch or a zero-norm vector.
///
/// Used to judge how relevant a skill's recorded failure mode is to the current problem:
/// a bad pattern whose embedding is close to the query is a failure the agent is about to
/// risk again. Stored and query embeddings are already unit-normalized, so this is a dot
/// product in practice, but the full form keeps it correct for any input.
pub(crate) fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut norm_a = 0.0;
    let mut norm_b = 0.0;
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

/// The bad-pattern rank penalty: `1 / (1 + weight * count)`, in `(0, 1]`.
///
/// A skill with no query-relevant failure modes keeps its full score (`count = 0` → `1.0`);
/// each relevant failure mode shrinks the score multiplicatively, bounded and monotonic, so a
/// known-risky skill sinks below an equally-matched clean one without ever going negative. The
/// `weight` knob sets how hard each relevant pattern bites.
pub(crate) fn bad_pattern_penalty(weight: f64, count: usize) -> f64 {
    1.0 / (1.0 + weight * count as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A0: f64 = 1.0;
    const B0: f64 = 1.0;

    #[test]
    fn unproven_skill_is_neutral() {
        assert!((reliability(A0, B0, 0, 0) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn one_success_is_two_thirds_not_one() {
        assert!((reliability(A0, B0, 1, 0) - 2.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn one_failure_is_one_third() {
        assert!((reliability(A0, B0, 0, 1) - 1.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn evidence_pulls_toward_the_empirical_rate() {
        // 50/2 approaches but never reaches 1.0; far above the neutral prior.
        let r = reliability(A0, B0, 50, 2);
        assert!(r > 0.9 && r < 1.0, "got {r}");
        assert!((r - 51.0 / 54.0).abs() < 1e-12);
    }

    #[test]
    fn reliability_is_monotonic() {
        let base = reliability(A0, B0, 5, 5);
        assert!(reliability(A0, B0, 6, 5) > base, "a success raises it");
        assert!(reliability(A0, B0, 5, 6) < base, "a failure lowers it");
    }

    #[test]
    fn a_stronger_prior_shrinks_toward_one_half() {
        // A heavier symmetric prior keeps a lopsided record closer to neutral.
        let weak = reliability(1.0, 1.0, 3, 0);
        let strong = reliability(10.0, 10.0, 3, 0);
        assert!(
            strong < weak,
            "strong prior is more conservative: {strong} < {weak}"
        );
        assert!(strong > 0.5);
    }

    #[test]
    fn rrf_ranks_a_key_high_in_both_lists_first() {
        // `1` is rank 0 in both; `2` only in list one; `3` only in list two.
        let list_one = [1u64, 2];
        let list_two = [1u64, 3];
        let fused = rrf(
            &[
                WeightedRanking {
                    weight: 1.0,
                    nodes: &list_one,
                },
                WeightedRanking {
                    weight: 1.0,
                    nodes: &list_two,
                },
            ],
            60.0,
        );
        assert_eq!(fused[0].0, 1, "the key ranked in both lists wins");
        assert_eq!(fused.len(), 3);
    }

    #[test]
    fn rrf_is_order_independent() {
        let l1 = [7u64, 9];
        let l2 = [9u64];
        let forward = rrf(
            &[
                WeightedRanking {
                    weight: 1.0,
                    nodes: &l1,
                },
                WeightedRanking {
                    weight: 2.0,
                    nodes: &l2,
                },
            ],
            60.0,
        );
        let reversed = rrf(
            &[
                WeightedRanking {
                    weight: 2.0,
                    nodes: &l2,
                },
                WeightedRanking {
                    weight: 1.0,
                    nodes: &l1,
                },
            ],
            60.0,
        );
        assert_eq!(forward, reversed);
    }

    #[test]
    fn cosine_of_identical_unit_vectors_is_one() {
        assert!((cosine(&[1.0, 0.0, 0.0, 0.0], &[1.0, 0.0, 0.0, 0.0]) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn cosine_of_orthogonal_vectors_is_zero() {
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-12);
    }

    #[test]
    fn cosine_handles_zero_norm_and_length_mismatch() {
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0, 0.0]), 0.0);
    }

    #[test]
    fn bad_pattern_penalty_is_bounded_and_monotonic() {
        assert!(
            (bad_pattern_penalty(0.5, 0) - 1.0).abs() < 1e-12,
            "no patterns, no penalty"
        );
        let one = bad_pattern_penalty(0.5, 1);
        let two = bad_pattern_penalty(0.5, 2);
        assert!((one - 1.0 / 1.5).abs() < 1e-12);
        assert!(two < one && one < 1.0, "more relevant patterns bite harder");
        assert!(
            two > 0.0,
            "the penalty never drives the score to or below zero"
        );
    }

    #[test]
    fn a_zero_weight_signal_is_elided() {
        let live = [1u64];
        let dead = [2u64];
        let fused = rrf(
            &[
                WeightedRanking {
                    weight: 1.0,
                    nodes: &live,
                },
                WeightedRanking {
                    weight: 0.0,
                    nodes: &dead,
                },
            ],
            60.0,
        );
        assert_eq!(fused.len(), 1, "the zero-weight signal contributes nothing");
        assert_eq!(fused[0].0, 1);
    }
}
