//! Drift-detection configuration (05 §1, M5.T05).
//!
//! Its own module because the drift posture is a coherent unit: the master
//! off-switch, the behavior-sample bounds the detector reads over, the warning
//! threshold, and the cooling-window knobs for core-proximate facts. Off by default —
//! a deployment that never sets it runs no sweep, stamps no cooling, and pays
//! nothing, the same all-defaults-inert posture as forgetting and reliability.
//!
//! The host maps these knobs into the engine's drift policy (which re-validates its
//! own copy), the same host-side indirection as
//! [`ForgettingConfig`](crate::ForgettingConfig) — no crate below the host takes a
//! config dependency. None of these thresholds is among the binding red-team
//! thresholds (07 §5): they are deployment-tunable, and the defaults are deliberately
//! conservative starting points, not calibrated values (calibration is post-1.0 bench
//! work).

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// Drift-detection posture (05 §1, M5.T05): whether the off-cursor drift sweep and
/// the cooling stamp run at all, and the bounds they act under.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DriftConfig {
    /// Master off-switch. When false the engine constructs no detector: the sweep
    /// returns an empty report, no warning is emitted, and no fact is ever cooled.
    pub enabled: bool,
    /// The per-block warning bar: a drift score at or above this emits a
    /// `drift_warning` audit row. In `(0, 1]`; a score can never exceed `1`, so a
    /// threshold of exactly `1` demands the maximum measurable drift.
    pub drift_threshold: f64,
    /// How far back "recent agent behavior" reaches, in seconds from the
    /// caller-supplied sweep instant. Default seven days. Validated non-zero when
    /// enabled.
    pub behavior_window_secs: u64,
    /// The cap on episodes sampled per namespace (most-recent first) — the cost
    /// bound. Validated non-zero when enabled.
    pub behavior_sample_size: usize,
    /// The floor under the sample: fewer embedded episodes than this and the block
    /// is skipped (counted in the report), never scored — the detector does not
    /// guess from a handful of turns. Validated `1..=behavior_sample_size`.
    pub min_sample_size: usize,
    /// How long a core-proximate fact's rank-time trust stays reduced, in seconds
    /// from its cooling stamp. Default seven days; hosts should keep it at or above
    /// their sweep cadence so the detector gets its look before the cooling lapses.
    /// Validated non-zero when enabled.
    pub cooling_window_secs: u64,
    /// The multiplier applied to a cooled fact's trust at rank time. In `(0, 1]`;
    /// `1` is a no-op posture, and zero is rejected — cooling reduces influence, it
    /// never erases a fact's rank entirely.
    pub cooling_factor: f64,
    /// The cosine bar for "proximate to a core block": a new fact at or above this
    /// similarity to any high-trust live core block is cooled. In `(0, 1]`.
    pub core_proximity_threshold: f64,
    /// The trust bar a core block must meet before proximity to it cools anything,
    /// mirroring the contradiction detector's high-trust gate. In `(0, 1]`.
    pub high_trust_threshold: f64,
}

impl Default for DriftConfig {
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

impl DriftConfig {
    /// Validate the posture when enabled, fail-closed with the offending key named.
    ///
    /// # Errors
    /// Returns [`ConfigError`] for an out-of-range threshold or factor, a zero
    /// window or sample bound, or a sample floor above the sample cap.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.enabled {
            return Ok(());
        }
        for (key, value) in [
            ("drift.drift_threshold", self.drift_threshold),
            ("drift.cooling_factor", self.cooling_factor),
            (
                "drift.core_proximity_threshold",
                self.core_proximity_threshold,
            ),
            ("drift.high_trust_threshold", self.high_trust_threshold),
        ] {
            if !value.is_finite() || value <= 0.0 || value > 1.0 {
                return Err(ConfigError::invalid(key, "must be in the range (0, 1]"));
            }
        }
        for (key, value) in [
            ("drift.behavior_window_secs", self.behavior_window_secs),
            ("drift.cooling_window_secs", self.cooling_window_secs),
        ] {
            if value == 0 {
                return Err(ConfigError::invalid(
                    key,
                    "must be greater than zero when drift detection is enabled",
                ));
            }
        }
        if self.behavior_sample_size == 0 {
            return Err(ConfigError::invalid(
                "drift.behavior_sample_size",
                "must be greater than zero when drift detection is enabled",
            ));
        }
        if self.min_sample_size == 0 || self.min_sample_size > self.behavior_sample_size {
            return Err(ConfigError::invalid(
                "drift.min_sample_size",
                "must be at least 1 and at most drift.behavior_sample_size",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_posture_is_off_and_validates() {
        let config = DriftConfig::default();
        assert!(
            !config.enabled,
            "off by default; an unset deployment pays nothing"
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn disabled_skips_validation_like_its_siblings() {
        let config = DriftConfig {
            enabled: false,
            drift_threshold: 7.0,
            ..DriftConfig::default()
        };
        assert!(config.validate().is_ok(), "inert values are not policed");
    }

    #[test]
    fn enabled_bounds_every_knob() {
        let on = DriftConfig {
            enabled: true,
            ..DriftConfig::default()
        };
        assert!(on.validate().is_ok(), "the defaults are a sound posture");

        for (mutate, key) in [
            (
                Box::new(|c: &mut DriftConfig| c.drift_threshold = 0.0) as Box<dyn Fn(&mut _)>,
                "drift_threshold zero",
            ),
            (
                Box::new(|c: &mut DriftConfig| c.drift_threshold = 1.1),
                "drift_threshold above one",
            ),
            (
                Box::new(|c: &mut DriftConfig| c.cooling_factor = 0.0),
                "cooling_factor zero erases rank",
            ),
            (
                Box::new(|c: &mut DriftConfig| c.core_proximity_threshold = f64::NAN),
                "non-finite proximity",
            ),
            (
                Box::new(|c: &mut DriftConfig| c.high_trust_threshold = -0.5),
                "negative trust bar",
            ),
            (
                Box::new(|c: &mut DriftConfig| c.behavior_window_secs = 0),
                "zero behavior window",
            ),
            (
                Box::new(|c: &mut DriftConfig| c.cooling_window_secs = 0),
                "zero cooling window",
            ),
            (
                Box::new(|c: &mut DriftConfig| c.behavior_sample_size = 0),
                "zero sample cap",
            ),
            (
                Box::new(|c: &mut DriftConfig| c.min_sample_size = 0),
                "zero sample floor",
            ),
            (
                Box::new(|c: &mut DriftConfig| {
                    c.min_sample_size = c.behavior_sample_size + 1;
                }),
                "floor above cap",
            ),
        ] {
            let mut config = on.clone();
            mutate(&mut config);
            assert!(config.validate().is_err(), "{key} must be rejected");
        }
    }
}
