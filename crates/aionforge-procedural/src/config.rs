//! Procedural-memory tuning knobs (05; M3.T04).
//!
//! The retrieval defaults mirror the retrieval path's validated rank-fusion settings (03 §2),
//! and the reliability prior is the weak, uninformative Beta(1,1) so an unproven skill is
//! neither boosted nor buried. Every knob is range-checked by [`ProceduralConfig::validate`] so
//! a misconfiguration is caught at construction, not silently mis-ranking at query time. The
//! knobs are surfaced so the M7 retrieval benchmarks can sweep them without a code change.

/// Tuning for skill retrieval and reliability weighting.
#[derive(Debug, Clone, PartialEq)]
pub struct ProceduralConfig {
    /// RRF smoothing constant for fusing the problem-embedding and description signals
    /// (03 §2; the validated low-tuning default is 60).
    pub rrf_k: f64,
    /// Mode weight on the vector (problem-embedding) signal. Zero elides it.
    pub vector_weight: f64,
    /// Mode weight on the lexical (description BM25) signal. Zero elides it.
    pub text_weight: f64,
    /// Candidate over-fetch factor: each signal fetches `k * candidate_multiplier` before
    /// reliability re-ranking, so a reliable-but-slightly-less-similar skill can still surface
    /// above an unproven top match. At least 1.
    pub candidate_multiplier: usize,
    /// Beta prior α₀ for the reliability posterior. The default weak prior is `1.0` (Beta(1,1),
    /// uniform): an unproven 0/0 skill scores a neutral `0.5`.
    pub prior_alpha: f64,
    /// Beta prior β₀ for the reliability posterior; default `1.0`.
    pub prior_beta: f64,
    /// How hard each query-relevant bad pattern shrinks a skill's rank score, via the penalty
    /// `1 / (1 + bad_pattern_weight * count)`. Default `0.5`: one relevant failure mode multiplies
    /// the score by `2/3`. Zero disables the bad-pattern penalty (patterns still surface).
    pub bad_pattern_weight: f64,
    /// Cosine-similarity floor, in `[0, 1]`, above which a skill's linked bad pattern counts as
    /// relevant to the current problem and contributes to the penalty. Default `0.7`.
    pub bad_pattern_similarity_threshold: f64,
}

impl Default for ProceduralConfig {
    fn default() -> Self {
        Self {
            rrf_k: 60.0,
            vector_weight: 1.0,
            text_weight: 1.0,
            candidate_multiplier: 4,
            prior_alpha: 1.0,
            prior_beta: 1.0,
            bad_pattern_weight: 0.5,
            bad_pattern_similarity_threshold: 0.7,
        }
    }
}

impl ProceduralConfig {
    /// Check every knob is in range.
    ///
    /// # Errors
    /// Returns a message naming the offending knob when: `rrf_k` is not finite and positive
    /// (the RRF denominator must stay positive); a signal weight is not finite and non-negative,
    /// or both weights are zero (which would leave no retrieval signal at all);
    /// `candidate_multiplier` is zero (it must over-fetch at least `k`); or a Beta prior is not
    /// finite and positive (a non-positive pseudo-count has no posterior meaning).
    pub fn validate(&self) -> Result<(), String> {
        if !self.rrf_k.is_finite() || self.rrf_k <= 0.0 {
            return Err(format!(
                "procedural.rrf_k must be a finite positive value, got {}",
                self.rrf_k
            ));
        }
        for (name, weight) in [
            ("procedural.vector_weight", self.vector_weight),
            ("procedural.text_weight", self.text_weight),
        ] {
            if !weight.is_finite() || weight < 0.0 {
                return Err(format!(
                    "{name} must be a finite non-negative value, got {weight}"
                ));
            }
        }
        if self.vector_weight == 0.0 && self.text_weight == 0.0 {
            return Err(
                "procedural.vector_weight and procedural.text_weight cannot both be zero — \
                 retrieval would have no signal"
                    .to_string(),
            );
        }
        if self.candidate_multiplier == 0 {
            return Err("procedural.candidate_multiplier must be at least 1".to_string());
        }
        for (name, prior) in [
            ("procedural.prior_alpha", self.prior_alpha),
            ("procedural.prior_beta", self.prior_beta),
        ] {
            if !prior.is_finite() || prior <= 0.0 {
                return Err(format!(
                    "{name} must be a finite positive value, got {prior}"
                ));
            }
        }
        if !self.bad_pattern_weight.is_finite() || self.bad_pattern_weight < 0.0 {
            return Err(format!(
                "procedural.bad_pattern_weight must be a finite non-negative value, got {}",
                self.bad_pattern_weight
            ));
        }
        if !self.bad_pattern_similarity_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.bad_pattern_similarity_threshold)
        {
            return Err(format!(
                "procedural.bad_pattern_similarity_threshold must be a finite value in [0, 1], got {}",
                self.bad_pattern_similarity_threshold
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::ProceduralConfig;

    #[test]
    fn the_default_config_validates() {
        ProceduralConfig::default()
            .validate()
            .expect("default is in range");
    }

    #[test]
    fn an_out_of_range_rrf_k_is_rejected() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let config = ProceduralConfig {
                rrf_k: bad,
                ..ProceduralConfig::default()
            };
            assert!(config.validate().is_err(), "rrf_k {bad} should be rejected");
        }
    }

    #[test]
    fn both_weights_zero_is_rejected() {
        let config = ProceduralConfig {
            vector_weight: 0.0,
            text_weight: 0.0,
            ..ProceduralConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn one_zero_weight_is_allowed() {
        let config = ProceduralConfig {
            vector_weight: 0.0,
            ..ProceduralConfig::default()
        };
        config.validate().expect("a single live signal is fine");
    }

    #[test]
    fn a_zero_candidate_multiplier_is_rejected() {
        let config = ProceduralConfig {
            candidate_multiplier: 0,
            ..ProceduralConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn a_non_positive_prior_is_rejected() {
        for bad in [0.0, -0.5, f64::NAN, f64::INFINITY] {
            let alpha = ProceduralConfig {
                prior_alpha: bad,
                ..ProceduralConfig::default()
            };
            assert!(alpha.validate().is_err(), "alpha {bad} should be rejected");
            let beta = ProceduralConfig {
                prior_beta: bad,
                ..ProceduralConfig::default()
            };
            assert!(beta.validate().is_err(), "beta {bad} should be rejected");
        }
    }

    #[test]
    fn a_negative_bad_pattern_weight_is_rejected() {
        for bad in [-0.1, f64::NAN, f64::INFINITY] {
            let config = ProceduralConfig {
                bad_pattern_weight: bad,
                ..ProceduralConfig::default()
            };
            assert!(
                config.validate().is_err(),
                "weight {bad} should be rejected"
            );
        }
        // Zero is allowed: it disables the penalty.
        let zero = ProceduralConfig {
            bad_pattern_weight: 0.0,
            ..ProceduralConfig::default()
        };
        zero.validate().expect("a zero weight disables the penalty");
    }

    #[test]
    fn an_out_of_range_bad_pattern_threshold_is_rejected() {
        for bad in [-0.1, 1.1, f64::NAN, f64::INFINITY] {
            let config = ProceduralConfig {
                bad_pattern_similarity_threshold: bad,
                ..ProceduralConfig::default()
            };
            assert!(
                config.validate().is_err(),
                "threshold {bad} should be rejected"
            );
        }
    }
}
