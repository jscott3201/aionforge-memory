//! The reliability-weighted Beta posterior over a candidate fact's correctness (06 §4–§5).
//!
//! Quorum promotion asks one question: given the signed attestations on a team fact and
//! how reliable each attester is, how confident is the substrate that the fact is correct?
//! This module answers it with a Beta posterior, the natural conjugate for a Bernoulli
//! "correct / not correct" event and the same shape the substrate already stores per agent
//! ([`TrustCategory`](crate::nodes::agent::TrustCategory) carries `alpha`/`beta`).
//!
//! The math is pure and I/O-free so the orchestrator (L2) and its tests share one
//! implementation; the trust layer does the graph reads that feed it.

/// The Beta posterior over "this candidate fact is correct," from a prior and a set of
/// per-attester reliabilities.
///
/// Each attester contributes its reliability `r` in `[0, 1]` as fractional evidence: `r`
/// toward correctness and `1 - r` toward incorrectness. So over a `Beta(prior_alpha,
/// prior_beta)` prior,
///
/// ```text
/// alpha = prior_alpha + Σ r_a
/// beta  = prior_beta  + Σ (1 - r_a)
/// score = alpha / (alpha + beta)
/// ```
///
/// Because `r + (1 - r) = 1`, the denominator grows by exactly one per attester, so the
/// posterior **asymptotes to the attesters' quality mean and can never be driven to `1.0`
/// by count alone**. That keeps the two promotion gates orthogonal — the count gate `k`
/// (enough independent evidence) and the belief gate `threshold` (high enough posterior)
/// — so a swarm of merely-above-average attesters cannot buy arbitrary confidence. This is
/// the sybil-resistance property quorum promotion needs (07 §T5; the M6.T04 "zero
/// malicious skills promote" ceiling). A log-odds pooling would instead saturate toward
/// `1.0` with count, letting a large enough mediocre quorum clear any threshold.
///
/// Each reliability is sanitized on the way in: a non-finite value is read as `0.5` (the
/// no-information neutral, which moves the mean toward neither pole), and a finite value is
/// clamped to `[0, 1]`, so a corrupt out-of-range score can never push a pseudo-count
/// negative. The caller passes the reliabilities in a canonical order (the orchestrator
/// sorts attesters by id), so the floating-point summation is byte-identical on replay.
///
/// Returns `(alpha, beta, score)`. `alpha + beta` is always `>= prior_alpha + prior_beta >
/// 0` (the caller validates positive priors), so the division is never `0 / 0`.
#[must_use]
pub fn beta_posterior(prior_alpha: f64, prior_beta: f64, reliabilities: &[f64]) -> (f64, f64, f64) {
    let mut alpha = prior_alpha;
    let mut beta = prior_beta;
    for &raw in reliabilities {
        let r = if raw.is_finite() {
            raw.clamp(0.0, 1.0)
        } else {
            0.5
        };
        alpha += r;
        beta += 1.0 - r;
    }
    let score = alpha / (alpha + beta);
    (alpha, beta, score)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-12;

    #[test]
    fn no_attesters_yield_the_prior_mean() {
        let (alpha, beta, score) = beta_posterior(1.0, 1.0, &[]);
        assert!((alpha - 1.0).abs() < EPS);
        assert!((beta - 1.0).abs() < EPS);
        assert!((score - 0.5).abs() < EPS, "uniform prior mean is 0.5");
    }

    #[test]
    fn one_perfect_attester_over_a_uniform_prior_is_two_thirds() {
        let (_, _, score) = beta_posterior(1.0, 1.0, &[1.0]);
        assert!((score - 2.0 / 3.0).abs() < EPS);
    }

    #[test]
    fn one_fully_unreliable_attester_lowers_the_posterior() {
        let (_, _, score) = beta_posterior(1.0, 1.0, &[0.0]);
        assert!((score - 1.0 / 3.0).abs() < EPS, "r=0 adds only to beta");
    }

    #[test]
    fn a_swarm_of_mediocre_attesters_stays_bounded_by_their_mean() {
        // Ten attesters at r=0.6: alpha=1+6=7, beta=1+4=5, score=7/12 ~ 0.583.
        let (_, _, score) = beta_posterior(1.0, 1.0, &[0.6; 10]);
        assert!((score - 7.0 / 12.0).abs() < EPS);
        assert!(
            score < 0.95,
            "count alone cannot clear a 0.95 threshold (sybil bound)"
        );
        // A hundred of them barely moves it — the count gate and the belief gate are orthogonal.
        let (_, _, many) = beta_posterior(1.0, 1.0, &[0.6; 100]);
        assert!(
            many < 0.61,
            "still asymptotes to the quality mean, not to 1.0"
        );
    }

    #[test]
    fn high_reliability_attesters_can_clear_a_strict_threshold() {
        // Five attesters at r=0.99 over Beta(1,1): alpha=1+4.95=5.95, beta=1+0.05=1.05.
        let (_, _, score) = beta_posterior(1.0, 1.0, &[0.99; 5]);
        assert!(score > 0.84, "genuine high-reliability evidence accrues");
    }

    #[test]
    fn out_of_range_and_non_finite_reliabilities_are_sanitized() {
        // 1.5 clamps to 1.0; -0.3 clamps to 0.0; NaN reads as the 0.5 neutral.
        let (_, _, clamped) = beta_posterior(1.0, 1.0, &[1.5, -0.3, f64::NAN]);
        let (_, _, reference) = beta_posterior(1.0, 1.0, &[1.0, 0.0, 0.5]);
        assert!((clamped - reference).abs() < EPS);
        assert!(clamped.is_finite(), "a corrupt score never poisons the sum");
    }

    #[test]
    fn summation_is_order_independent_for_a_fixed_multiset() {
        let forward = beta_posterior(1.0, 1.0, &[0.2, 0.7, 0.9]);
        let reverse = beta_posterior(1.0, 1.0, &[0.9, 0.7, 0.2]);
        assert!((forward.2 - reverse.2).abs() < EPS);
    }
}
