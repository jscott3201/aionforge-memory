//! The engine-facing forgetting policy (05 §2, M5.T02).
//!
//! The host maps `aionforge-config`'s `ForgettingConfig` plus the decay section's tier
//! half-lives into this struct, the same indirection as the reliability policy, so
//! neither the engine nor this crate takes a config dependency. The half-lives ride
//! along because sweep-time decay must agree with rank-time decay — they are the same
//! two knobs, never re-declared — and they arrive regardless of whether rank-time decay
//! is enabled (the half-lives are always defined; `decay.enabled` only gates their
//! application at rank time).

/// Active-forgetting policy: the off-switch, the floors a candidate must sit below on
/// every axis, and the tier half-lives the decayed-importance axis reads.
///
/// Defaults are off and conservative, mirroring the config section: a memory written
/// with the default capture importance and trust is never a candidate until it has
/// genuinely faded.
#[derive(Debug, Clone, PartialEq)]
pub struct ForgettingPolicy {
    /// Master off-switch. When false the engine builds no [`Forgetter`](crate::Forgetter)
    /// and every forget surface is inert.
    pub enabled: bool,
    /// A candidate is low-importance only when its decayed importance sits below this.
    pub importance_floor: f64,
    /// A candidate is low-trust only when its per-memory trust scalar sits below this.
    pub trust_floor: f64,
    /// A candidate must be at least this old (seconds since ingestion) to be forgettable.
    pub min_age_secs: u64,
    /// Per-page candidate cap for the sweep.
    pub batch_cap: usize,
    /// Whether `BadPattern` records may be point-forgotten. Negative knowledge is
    /// protected by default.
    pub forget_bad_patterns: bool,
    /// Episodic-tier half-life in seconds, from the decay section.
    pub episodic_half_life_secs: f64,
    /// Semantic-tier half-life in seconds, from the decay section.
    pub semantic_half_life_secs: f64,
}

impl Default for ForgettingPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            importance_floor: 0.05,
            trust_floor: 0.30,
            min_age_secs: 2_592_000,
            batch_cap: 200,
            forget_bad_patterns: false,
            episodic_half_life_secs: 604_800.0,
            semantic_half_life_secs: 31_536_000.0,
        }
    }
}

impl ForgettingPolicy {
    /// Re-validate the engine's own copy of the policy, mirroring the config section's
    /// rules plus the half-life sanity this side owns. Vacuous when disabled.
    ///
    /// # Errors
    /// Returns a message naming the offending knob, the reliability-policy shape.
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        for (key, floor) in [
            ("forgetting importance_floor", self.importance_floor),
            ("forgetting trust_floor", self.trust_floor),
        ] {
            if !(floor.is_finite() && (0.0..=1.0).contains(&floor)) {
                return Err(format!(
                    "{key} must be a finite value in the range [0.0, 1.0]"
                ));
            }
        }
        if self.batch_cap == 0 {
            return Err("forgetting batch_cap must be greater than zero".to_string());
        }
        if self.importance_floor >= 1.0 && self.trust_floor >= 1.0 {
            return Err(
                "forgetting floors must not both sit at 1.0 (nearly every unpinned \
                 memory would become a sweep candidate)"
                    .to_string(),
            );
        }
        for (key, half_life) in [
            ("episodic half-life", self.episodic_half_life_secs),
            ("semantic half-life", self.semantic_half_life_secs),
        ] {
            // The pure decay function treats a non-positive half-life as inert (no
            // decay), which would silently turn the importance axis into "stored value
            // only"; reject the misconfiguration where it is visible instead.
            if !half_life.is_finite() || half_life <= 0.0 {
                return Err(format!(
                    "{key} must be a finite value greater than zero when forgetting is \
                     enabled"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_and_valid_when_enabled() {
        let policy = ForgettingPolicy::default();
        assert!(!policy.enabled);
        policy.validate().expect("off validates vacuously");
        let enabled = ForgettingPolicy {
            enabled: true,
            ..ForgettingPolicy::default()
        };
        enabled.validate().expect("enabled defaults validate");
    }

    #[test]
    fn a_disabled_policy_never_rejects() {
        let policy = ForgettingPolicy {
            enabled: false,
            importance_floor: f64::NAN,
            batch_cap: 0,
            episodic_half_life_secs: -1.0,
            ..ForgettingPolicy::default()
        };
        policy.validate().expect("inert values are never rejected");
    }

    #[test]
    fn drift_policy_defaults_are_off_and_valid_when_enabled() {
        let policy = DriftPolicy::default();
        assert!(!policy.enabled);
        policy.validate().expect("off validates vacuously");
        let enabled = DriftPolicy {
            enabled: true,
            ..DriftPolicy::default()
        };
        enabled.validate().expect("enabled defaults validate");
    }

    #[test]
    fn drift_policy_rejects_each_bad_knob_only_when_enabled() {
        let off = DriftPolicy {
            enabled: false,
            drift_threshold: f64::NAN,
            min_sample_size: 0,
            ..DriftPolicy::default()
        };
        off.validate().expect("inert values are never rejected");

        let base = DriftPolicy {
            enabled: true,
            ..DriftPolicy::default()
        };
        for bad in [
            DriftPolicy {
                drift_threshold: 0.0,
                ..base.clone()
            },
            DriftPolicy {
                cooling_factor: 1.5,
                ..base.clone()
            },
            DriftPolicy {
                core_proximity_threshold: f64::NAN,
                ..base.clone()
            },
            DriftPolicy {
                high_trust_threshold: -0.1,
                ..base.clone()
            },
            DriftPolicy {
                behavior_window_secs: 0,
                ..base.clone()
            },
            DriftPolicy {
                cooling_window_secs: 0,
                ..base.clone()
            },
            DriftPolicy {
                behavior_sample_size: 0,
                ..base.clone()
            },
            DriftPolicy {
                min_sample_size: 0,
                ..base.clone()
            },
            DriftPolicy {
                min_sample_size: 257,
                ..base.clone()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn enabled_validation_rejects_each_bad_knob() {
        let base = ForgettingPolicy {
            enabled: true,
            ..ForgettingPolicy::default()
        };
        for bad in [
            ForgettingPolicy {
                importance_floor: 1.5,
                ..base.clone()
            },
            ForgettingPolicy {
                trust_floor: f64::NAN,
                ..base.clone()
            },
            ForgettingPolicy {
                batch_cap: 0,
                ..base.clone()
            },
            ForgettingPolicy {
                importance_floor: 1.0,
                trust_floor: 1.0,
                ..base.clone()
            },
            ForgettingPolicy {
                episodic_half_life_secs: 0.0,
                ..base.clone()
            },
            ForgettingPolicy {
                semantic_half_life_secs: f64::INFINITY,
                ..base.clone()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?} must be rejected");
        }
    }
}

/// Right-to-erasure policy: the off-switch and the cascade caps (05 §3, M5.T03).
///
/// Deliberately a **separate switch** from [`ForgettingPolicy::enabled`]: forgetting is
/// reversible and erasure is not, so enabling the soft path must never silently arm the
/// destructive one. Defaults are off, with caps sized so a runaway derivation graph is
/// refused rather than destroyed in one oversized transaction — the cap values are
/// owner-ratified policy, not physics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErasurePolicy {
    /// Master off-switch. When false the engine builds no [`Eraser`](crate::Eraser)
    /// and every erase surface answers disabled.
    pub enabled: bool,
    /// The deepest derivation level a cascade may reach (the seed is depth 0).
    pub max_cascade_depth: usize,
    /// The most nodes one cascade may destroy, provenance records included.
    pub max_cascade_nodes: usize,
}

impl Default for ErasurePolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            max_cascade_depth: 32,
            max_cascade_nodes: 5_000,
        }
    }
}

impl ErasurePolicy {
    /// Re-validate the engine's own copy of the policy. Vacuous when disabled.
    ///
    /// # Errors
    /// Returns a message naming the offending knob, the reliability-policy shape.
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.max_cascade_depth == 0 {
            return Err("erasure.max_cascade_depth must be at least 1".to_string());
        }
        if self.max_cascade_nodes == 0 {
            return Err("erasure.max_cascade_nodes must be at least 1".to_string());
        }
        Ok(())
    }
}

/// Drift-detection policy (05 §1, M5.T05): the off-switch, the behavior-window and
/// sample bounds the detector reads over, the warning threshold, and the cooling
/// knobs for core-proximate facts.
///
/// The host maps `aionforge-config`'s `DriftConfig` into this struct, the same
/// indirection as the two policies above. Off by default: the engine builds no
/// [`DriftDetector`](crate::DriftDetector), the sweep reports empty, and no fact is
/// ever cooled.
#[derive(Debug, Clone, PartialEq)]
pub struct DriftPolicy {
    /// Master off-switch.
    pub enabled: bool,
    /// Per-block warning bar: a drift score at or above this crosses. In `(0, 1]`.
    pub drift_threshold: f64,
    /// How far back "recent agent behavior" reaches, in seconds from the
    /// caller-supplied sweep instant.
    pub behavior_window_secs: u64,
    /// The cap on episodes sampled per namespace (most-recent kept).
    pub behavior_sample_size: usize,
    /// The floor under the sample: fewer comparable episodes than this and the
    /// block skips, never scores.
    pub min_sample_size: usize,
    /// How long a cooled fact's rank-time trust stays reduced, in seconds from its
    /// stamp.
    pub cooling_window_secs: u64,
    /// The multiplier applied to a cooled fact's trust at rank time. In `(0, 1]`.
    pub cooling_factor: f64,
    /// The cosine bar for "proximate to a core block". In `(0, 1]`.
    pub core_proximity_threshold: f64,
    /// The trust bar a core block must meet before proximity to it cools anything.
    /// In `(0, 1]`.
    pub high_trust_threshold: f64,
}

impl Default for DriftPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            drift_threshold: 0.15,
            behavior_window_secs: 604_800,
            behavior_sample_size: 256,
            min_sample_size: 8,
            cooling_window_secs: 604_800,
            cooling_factor: 0.5,
            core_proximity_threshold: 0.75,
            high_trust_threshold: 0.7,
        }
    }
}

impl DriftPolicy {
    /// Re-validate the engine's own copy of the policy, mirroring `DriftConfig`'s
    /// rules. Vacuous when disabled.
    ///
    /// # Errors
    /// Returns a message naming the offending knob, the reliability-policy shape.
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        for (key, value) in [
            ("drift drift_threshold", self.drift_threshold),
            ("drift cooling_factor", self.cooling_factor),
            (
                "drift core_proximity_threshold",
                self.core_proximity_threshold,
            ),
            ("drift high_trust_threshold", self.high_trust_threshold),
        ] {
            if !value.is_finite() || value <= 0.0 || value > 1.0 {
                return Err(format!("{key} must be in the range (0, 1]"));
            }
        }
        for (key, value) in [
            ("drift behavior_window_secs", self.behavior_window_secs),
            ("drift cooling_window_secs", self.cooling_window_secs),
        ] {
            if value == 0 {
                return Err(format!("{key} must be greater than zero when enabled"));
            }
        }
        if self.behavior_sample_size == 0 {
            return Err("drift behavior_sample_size must be greater than zero when enabled".into());
        }
        if self.min_sample_size == 0 || self.min_sample_size > self.behavior_sample_size {
            return Err(
                "drift min_sample_size must be at least 1 and at most behavior_sample_size".into(),
            );
        }
        Ok(())
    }
}
