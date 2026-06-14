//! Cross-family consolidation-guard configuration (07 §3, M6.T01).
//!
//! Its own module because the guard posture is a coherent unit: the refuse-or-warn
//! mode and the deployment's declared consolidating family. There is deliberately
//! **no master off-switch** — the guard is substrate policy over every
//! inference-calling consolidation rule (07 §3: "enforced at the substrate, not
//! left to user code"), and it is inert until a host injects an inference-backed
//! (model-family-bearing) summarizer or link evolver, so an all-defaults
//! deployment pays nothing.
//!
//! The host maps these knobs into the engine's `ConsolidationGuardPolicy` (which
//! re-validates its own copy), the same host-side indirection as
//! [`DriftConfig`](crate::DriftConfig) — no crate below the host takes a config
//! dependency.

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// What the guard does when the consolidating family matches a writer's (or
/// cannot be verified): refuse the item, or proceed and audit a warning
/// (plan M6.T01: "refused (or warned, per config)").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardMode {
    /// Skip the offending item and audit the refusal — the default; a security
    /// guard defaults to its strong mode.
    #[default]
    Refuse,
    /// Proceed, but audit the same finding as a warning.
    Warn,
}

/// Cross-family guard posture (07 §3, M6.T01): the refuse-or-warn mode and the
/// declared consolidating family the startup single-family check compares
/// against.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ConsolidationGuardConfig {
    /// What a fired guard does: `refuse` (default) skips the item and audits;
    /// `warn` proceeds and audits the same finding.
    pub mode: GuardMode,
    /// The model family the deployment consolidates with. Feeds the startup
    /// single-family warning (07 §3); the per-call guard reads the
    /// summarizer/evolver's own declared identity and works whether or not
    /// this is set. `None` skips the startup check.
    pub declared_consolidator_family: Option<String>,
}

impl ConsolidationGuardConfig {
    /// Validate the posture, fail-closed with the offending key named.
    ///
    /// # Errors
    /// Returns [`ConfigError`] when the declared family is set but empty —
    /// an unverifiable declaration is worse than none (07 §3).
    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(family) = &self.declared_consolidator_family
            && family.trim().is_empty()
        {
            return Err(ConfigError::invalid(
                "consolidation_guard.declared_consolidator_family",
                "must be non-empty when set; omit it to skip the startup check",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_posture_is_refuse_and_validates() {
        let config = ConsolidationGuardConfig::default();
        assert_eq!(
            config.mode,
            GuardMode::Refuse,
            "a security guard defaults to its strong mode"
        );
        assert!(config.declared_consolidator_family.is_none());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn a_declared_family_must_be_non_empty() {
        let config = ConsolidationGuardConfig {
            declared_consolidator_family: Some("   ".to_string()),
            ..ConsolidationGuardConfig::default()
        };
        assert!(
            config.validate().is_err(),
            "an empty declaration is unverifiable, not a skip"
        );

        let config = ConsolidationGuardConfig {
            declared_consolidator_family: Some("claude-sonnet-4-6".to_string()),
            ..ConsolidationGuardConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn the_mode_round_trips_snake_case() {
        let config: ConsolidationGuardConfig =
            serde_json::from_str(r#"{"mode": "warn"}"#).expect("parse");
        assert_eq!(config.mode, GuardMode::Warn);
        let json = serde_json::to_string(&config).expect("serialize");
        assert!(json.contains(r#""mode":"warn""#));
        let back: ConsolidationGuardConfig = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(back, config);
    }
}
