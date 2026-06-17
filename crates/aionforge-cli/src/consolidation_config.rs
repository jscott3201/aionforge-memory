//! Host-side mapping from the layered `[consolidation]` config block into the engine's
//! deterministic consolidation types.
//!
//! This is the consolidation analogue of `host::consolidation_guard_policy`: it converts the
//! config crate's plain primitives (durations as `*_secs` seconds, thresholds as `f64`,
//! counts as `usize`) into the engine's
//! `aionforge_consolidate::{ConsolidationConfig, PassConfig}` — `*_secs` becoming a
//! [`std::time::Duration`] via [`Duration::from_secs`]. Kept in the CLI (not the config
//! crate) so the config crate takes no dependency on the consolidate/engine crates.
//!
//! The mapping is **default-preserving**: with an absent `[consolidation]` block (the
//! config-layer [`ConsolidationConfig::default`](aionforge_config::ConsolidationConfig), which
//! mirrors the engine defaults literal-for-literal except for the serve-only `enabled` switch),
//! `consolidation_settings` returns exactly
//! `(engine::ConsolidationConfig::default(), engine::PassConfig::default())`, so behavior is
//! unchanged. The serve-only `enabled` switch is intentionally ignored here and consumed by
//! `serve`; it is not part of the engine scheduler config. The detection predicate map is
//! **not** config-exposed; it is taken from the
//! engine `DetectionConfig::default()` (the conservative built-in ruleset) and only `enabled`
//! and `high_trust_threshold` are overridden, so the default predicate rules are preserved.
//! The defaults-equality is pinned by the crate's `tests` module.

use std::time::Duration;

use aionforge::{
    ConsolidationConfig as EngineConsolidationConfig, DetectionConfig, ExtractionConfig,
    InductionConfig, PassConfig, ResolutionConfig, SummarizationConfig,
};
use aionforge_config::Config;

/// Map the layered `[consolidation]` block into the engine's scheduler config and pass-level
/// tuning, the two values [`aionforge::Memory::consolidate_once`] consumes. Mirrors
/// `host::consolidation_guard_policy`; converts every `*_secs` primitive via
/// [`Duration::from_secs`].
pub(crate) fn consolidation_settings(config: &Config) -> (EngineConsolidationConfig, PassConfig) {
    let c = &config.consolidation;
    let scheduler = EngineConsolidationConfig {
        tick_interval: Duration::from_secs(c.tick_interval_secs),
        batch_size: c.batch_size,
        apply_timeout: Duration::from_secs(c.apply_timeout_secs),
        max_retries: c.max_retries,
        lag_ceiling: Duration::from_secs(c.lag_ceiling_secs),
    };
    let pass = PassConfig {
        extraction: ExtractionConfig {
            min_confidence: c.extraction.min_confidence,
            derived_trust_factor: c.extraction.derived_trust_factor,
        },
        resolution: ResolutionConfig {
            candidate_k: c.resolution.candidate_k,
            merge_threshold: c.resolution.merge_threshold,
        },
        // Start from the engine default so the conservative built-in predicate ruleset (which
        // is not config-exposed) is preserved; override only the two exposed knobs.
        detection: DetectionConfig {
            enabled: c.detection.enabled,
            high_trust_threshold: c.detection.high_trust_threshold,
            ..DetectionConfig::default()
        },
        summarization: SummarizationConfig {
            enabled: c.summarization.enabled,
            min_facts: c.summarization.min_facts,
            min_entities: c.summarization.min_entities,
            entity_retention_threshold: c.summarization.entity_retention_threshold,
            confidence_floor: c.summarization.confidence_floor,
        },
        induction: InductionConfig {
            enabled: c.induction.enabled,
            recurrence_window: c.induction.recurrence_window,
            repetition_threshold: c.induction.repetition_threshold,
            min_distinct_tokens: c.induction.min_distinct_tokens,
            min_body_chars: c.induction.min_body_chars,
            max_body_chars: c.induction.max_body_chars,
            name_prefix: c.induction.name_prefix.clone(),
        },
    };
    (scheduler, pass)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_block_maps_to_the_engine_defaults_knob_for_knob() {
        // This is the no-behavior-change guarantee: with an absent [consolidation] block, the
        // mapped engine types equal `::default()` for EVERY knob. If the config-layer Default
        // ever drifts from the engine Default, this fails.
        let config = Config::default();
        let (scheduler, pass) = consolidation_settings(&config);

        let default_scheduler = EngineConsolidationConfig::default();
        assert_eq!(scheduler.tick_interval, default_scheduler.tick_interval);
        assert_eq!(scheduler.batch_size, default_scheduler.batch_size);
        assert_eq!(scheduler.apply_timeout, default_scheduler.apply_timeout);
        assert_eq!(scheduler.max_retries, default_scheduler.max_retries);
        assert_eq!(scheduler.lag_ceiling, default_scheduler.lag_ceiling);
        // The whole scheduler config is `PartialEq + Eq`, so pin it as one value too.
        assert_eq!(scheduler, default_scheduler);

        let default_pass = PassConfig::default();
        // Extraction.
        assert_eq!(
            pass.extraction.min_confidence,
            default_pass.extraction.min_confidence
        );
        assert_eq!(
            pass.extraction.derived_trust_factor,
            default_pass.extraction.derived_trust_factor
        );
        // Resolution.
        assert_eq!(
            pass.resolution.candidate_k,
            default_pass.resolution.candidate_k
        );
        assert_eq!(
            pass.resolution.merge_threshold,
            default_pass.resolution.merge_threshold
        );
        // Detection — including the non-exposed predicate map, preserved from the engine default.
        assert_eq!(pass.detection.enabled, default_pass.detection.enabled);
        assert_eq!(
            pass.detection.high_trust_threshold,
            default_pass.detection.high_trust_threshold
        );
        assert_eq!(
            pass.detection.predicates, default_pass.detection.predicates,
            "the non-exposed predicate ruleset must be preserved from the engine default"
        );
        // Summarization.
        assert_eq!(
            pass.summarization.enabled,
            default_pass.summarization.enabled
        );
        assert_eq!(
            pass.summarization.min_facts,
            default_pass.summarization.min_facts
        );
        assert_eq!(
            pass.summarization.min_entities,
            default_pass.summarization.min_entities
        );
        assert_eq!(
            pass.summarization.entity_retention_threshold,
            default_pass.summarization.entity_retention_threshold
        );
        assert_eq!(
            pass.summarization.confidence_floor,
            default_pass.summarization.confidence_floor
        );
        // Induction (off by default).
        assert!(!pass.induction.enabled);
        assert_eq!(
            pass.induction.recurrence_window,
            default_pass.induction.recurrence_window
        );
        assert_eq!(
            pass.induction.repetition_threshold,
            default_pass.induction.repetition_threshold
        );
        assert_eq!(
            pass.induction.min_distinct_tokens,
            default_pass.induction.min_distinct_tokens
        );
        assert_eq!(
            pass.induction.min_body_chars,
            default_pass.induction.min_body_chars
        );
        assert_eq!(
            pass.induction.max_body_chars,
            default_pass.induction.max_body_chars
        );
        assert_eq!(
            pass.induction.name_prefix,
            default_pass.induction.name_prefix
        );
        // And the whole PassConfig as one value (it is `PartialEq`).
        assert_eq!(pass, default_pass);
    }

    #[test]
    fn overrides_flow_through_with_unit_conversion() {
        let mut config = Config::default();
        config.consolidation.enabled = true;
        config.consolidation.tick_interval_secs = 11;
        config.consolidation.batch_size = 7;
        config.consolidation.apply_timeout_secs = 90;
        config.consolidation.summarization.min_facts = 2;
        config.consolidation.summarization.confidence_floor = 0.5;
        config.consolidation.extraction.min_confidence = 0.85;
        config.consolidation.extraction.derived_trust_factor = 0.7;
        config.consolidation.induction.enabled = true;
        config.consolidation.induction.name_prefix = "skill/".to_string();
        config.validate().expect("the overridden block validates");

        let (scheduler, pass) = consolidation_settings(&config);
        assert_eq!(scheduler.tick_interval, Duration::from_secs(11));
        assert_eq!(scheduler.batch_size, 7);
        assert_eq!(scheduler.apply_timeout, Duration::from_secs(90));
        assert_eq!(pass.summarization.min_facts, 2);
        assert_eq!(pass.summarization.confidence_floor, 0.5);
        assert_eq!(
            pass.extraction.min_confidence, 0.85,
            "the host carried the extraction confidence floor"
        );
        assert_eq!(
            pass.extraction.derived_trust_factor, 0.7,
            "the host carried the derived-trust discount"
        );
        assert!(
            pass.induction.enabled,
            "the host carried induction.enabled=true"
        );
        assert_eq!(pass.induction.name_prefix, "skill/");
        // The non-overridden detection predicate map is still the engine default.
        assert_eq!(
            pass.detection.predicates,
            DetectionConfig::default().predicates
        );
    }
}
