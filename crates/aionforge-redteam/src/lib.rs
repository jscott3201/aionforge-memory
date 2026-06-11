//! Security red-team probe reports.
//!
//! The crate is deliberately small: it gives the M6/M8 gates one structured report
//! shape and keeps probe thresholds explicit. The probes themselves live in normal
//! Rust tests so a failed probe fails CI.

use serde::{Deserialize, Serialize};

/// The red-team task that introduced the report harness.
pub const M6_T04: &str = "M6.T04";

/// M6.T04's binding ceiling for structural injection/poisoning probes.
///
/// This is intentionally zero for this slice: a raw tag breakout, query-reflection
/// write, or skill-to-global promotion is a security failure, not a rate to tune.
pub const M6_T04_ATTACK_SUCCESS_CEILING: f64 = 0.0;

/// Whether a probe cleared its binding acceptance threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    /// The observed attack success rate is at or below the binding ceiling.
    Passed,
    /// The observed attack success rate exceeded the binding ceiling.
    Failed,
}

/// A structured red-team report for an attack-success-rate probe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeReport {
    /// Milestone task, for example `M6.T04`.
    pub task: String,
    /// Stable probe name.
    pub probe: String,
    /// Complete denominator. Nothing out-of-scope is silently subtracted.
    pub attempts: u64,
    /// Attempts that achieved the attack goal against Aionforge.
    pub attack_successes: u64,
    /// Attempts that would succeed against the naive baseline used for comparison.
    pub naive_successes: u64,
    /// Binding maximum attack-success rate.
    pub ceiling: f64,
    /// `attack_successes / attempts`, or `0.0` when there were no attempts.
    pub attack_success_rate: f64,
    /// `naive_successes / attempts`, or `0.0` when there were no attempts.
    pub naive_success_rate: f64,
    /// Threshold verdict.
    pub status: ProbeStatus,
}

impl ProbeReport {
    /// Build a rate report from raw counts.
    #[must_use]
    pub fn rate(
        task: impl Into<String>,
        probe: impl Into<String>,
        attempts: u64,
        attack_successes: u64,
        naive_successes: u64,
        ceiling: f64,
    ) -> Self {
        let attack_success_rate = rate(attack_successes, attempts);
        let naive_success_rate = rate(naive_successes, attempts);
        let status = if attack_success_rate <= ceiling {
            ProbeStatus::Passed
        } else {
            ProbeStatus::Failed
        };
        Self {
            task: task.into(),
            probe: probe.into(),
            attempts,
            attack_successes,
            naive_successes,
            ceiling,
            attack_success_rate,
            naive_success_rate,
            status,
        }
    }

    /// True when the report cleared its binding threshold.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.status == ProbeStatus::Passed
    }

    /// Serialize the report as a compact JSON value for test failure messages or release gates.
    ///
    /// # Errors
    /// Returns an error only if serde cannot serialize this report shape.
    pub fn to_json(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(self)
    }
}

fn rate(count: u64, attempts: u64) -> f64 {
    if attempts == 0 {
        0.0
    } else {
        count as f64 / attempts as f64
    }
}

#[cfg(test)]
mod tests {
    use super::{M6_T04_ATTACK_SUCCESS_CEILING, ProbeReport, ProbeStatus};

    #[test]
    fn rate_report_keeps_the_full_denominator() {
        let report = ProbeReport::rate("M6.T04", "sample", 4, 1, 4, 0.25);

        assert_eq!(report.attack_success_rate, 0.25);
        assert_eq!(report.naive_success_rate, 1.0);
        assert_eq!(report.status, ProbeStatus::Passed);
    }

    #[test]
    fn m6t04_zero_ceiling_fails_on_any_success() {
        let report = ProbeReport::rate("M6.T04", "sample", 2, 1, 2, M6_T04_ATTACK_SUCCESS_CEILING);

        assert!(!report.passed());
        assert_eq!(report.status, ProbeStatus::Failed);
    }
}
