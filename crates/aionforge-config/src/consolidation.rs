//! Deterministic consolidation-pass configuration (write-and-consolidation §2-§3).
//!
//! Its own module because the consolidation posture is a coherent unit: the scheduler's
//! pacing/bounds plus the four pass-level tunings (entity resolution, supersession
//! detection, summarization, and skill induction). The shipped consolidation path is the
//! deterministic rule set; these knobs only tune *how much* it does per tick and *how
//! conservative* each pass is — they never reach for an inference model.
//!
//! This struct holds **plain primitives only** (durations as `*_secs` seconds, thresholds
//! as `f64`, counts as `usize`, flags as `bool`, the induced-skill prefix as a `String`),
//! so the config crate keeps no dependency on `aionforge-consolidate`. The host maps these
//! primitives into the engine's `aionforge_consolidate::{ConsolidationConfig, PassConfig}`
//! (converting `*_secs` via [`std::time::Duration::from_secs`]), the same host-side
//! indirection as [`DriftConfig`](crate::DriftConfig) into the drift policy — no crate below
//! the host takes a config dependency.
//!
//! The [`Default`] impl reproduces the **exact** engine defaults from
//! `aionforge-consolidate/src/config.rs`, so an absent `[consolidation]` block changes
//! nothing: induction stays off, summarization stays on with its conservative floors, and
//! the scheduler keeps its 5s tick / 32-episode batch. (The defaults-equality is pinned by
//! the host's `consolidation_config` mapping test, which asserts the mapped engine types
//! equal `::default()` knob for knob.)
//!
//! # Example
//!
//! A full `[consolidation]` block (every key is optional; shown with the default values,
//! then two illustrative overrides — induction turned on and the summarization floors
//! lowered):
//!
//! ```toml
//! [consolidation]
//! # Scheduler pacing/bounds (all durations in seconds).
//! tick_interval_secs = 5     # how often the loop wakes to drain work
//! batch_size = 32            # most episodes one tick takes (per-tick concurrency bound)
//! apply_timeout_secs = 30    # wall-clock budget for one pass over one episode
//! max_retries = 5            # transient failures an episode may accrue before it fails
//! lag_ceiling_secs = 5       # warn when the oldest pending episode is older than this
//!
//! [consolidation.extraction]
//! min_confidence = 0.8        # rules below this confidence never fire (recall-preserving floor)
//! derived_trust_factor = 0.9  # discount a derived fact's trust so it ranks below its episode
//!
//! [consolidation.resolution]
//! candidate_k = 8            # candidate entities each lexical/vector probe pulls
//! merge_threshold = 0.12     # cosine-distance ceiling under which a neighbor is the same entity
//!
//! [consolidation.detection]
//! enabled = true             # off => extraction-only
//! high_trust_threshold = 0.7 # incumbent trust at/above which a contradiction is quarantined
//!
//! [consolidation.summarization]
//! enabled = true                      # off => extraction/detection only
//! min_facts = 2                       # lowered from 3: summarize smaller clusters
//! min_entities = 2                    # fewest distinct entities before a cluster is summarized
//! entity_retention_threshold = 0.8    # lowered from 0.9: tolerate a slightly lossier summary
//! confidence_floor = 0.5              # lowered from 0.6: summarize lower-confidence clusters
//!
//! [consolidation.induction]
//! enabled = true             # turned ON (default is off): induce recurring procedures into skills
//! recurrence_window = 50     # most recent same-content episodes the recurrence probe counts
//! repetition_threshold = 3   # fewest exact-content recurrences before an episode is induced
//! min_distinct_tokens = 5    # fewest distinct whitespace tokens to be worth inducing
//! min_body_chars = 16        # fewest characters the content must carry
//! max_body_chars = 4096      # most characters an induced body may carry
//! name_prefix = "induced/"   # prefix on every induced skill name
//! ```

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// Deterministic consolidation posture (write-and-consolidation §2-§3): the background-loop
/// switch, the scheduler's pacing/bounds, and the four pass-level tunings.
///
/// All-defaults is today's behavior exactly: the serve-only `enabled` switch is off, and the
/// scheduler/pass primitives mirror the engine's `aionforge_consolidate` defaults knob for knob.
/// Plain primitives only; the host maps these into the engine's `ConsolidationConfig`/`PassConfig`
/// (see the module docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConsolidationConfig {
    /// Whether `aionforge serve` starts the autonomous background consolidation loop.
    ///
    /// Default `false` preserves the foreground-only posture: captures accrue in the raw backlog
    /// until an explicit `consolidate` tool call runs. Enabling this delegates consolidation to the
    /// serving process and makes the foreground tool a no-op/error to preserve the single writer.
    pub enabled: bool,
    /// How often the consolidation loop wakes to drain work, in seconds. Validated `> 0`.
    pub tick_interval_secs: u64,
    /// The most episodes one tick takes (the per-tick concurrency bound). Validated `>= 1`.
    pub batch_size: usize,
    /// The wall-clock budget for one pass over one episode, in seconds. Validated `> 0`.
    pub apply_timeout_secs: u64,
    /// How many transient failures an episode may accrue before it is marked failed.
    pub max_retries: u32,
    /// The steady-state lag ceiling, in seconds: the scheduler warns when the oldest pending
    /// episode is older than this. Validated `> 0`.
    pub lag_ceiling_secs: u64,
    /// Fact-extraction precision tuning (confidence floor, derived-trust discount).
    pub extraction: ExtractionSettings,
    /// Entity-resolution tuning.
    pub resolution: ResolutionSettings,
    /// Supersession/contradiction detection tuning.
    pub detection: DetectionSettings,
    /// Summary-note tuning.
    pub summarization: SummarizationSettings,
    /// Skill-induction tuning (off by default).
    pub induction: InductionSettings,
}

impl Default for ConsolidationConfig {
    fn default() -> Self {
        // `enabled` is serve-only; the rest mirrors
        // `aionforge_consolidate::ConsolidationConfig::default()` exactly.
        Self {
            enabled: false,
            tick_interval_secs: 5,
            batch_size: 32,
            apply_timeout_secs: 30,
            max_retries: 5,
            lag_ceiling_secs: 5,
            extraction: ExtractionSettings::default(),
            resolution: ResolutionSettings::default(),
            detection: DetectionSettings::default(),
            summarization: SummarizationSettings::default(),
            induction: InductionSettings::default(),
        }
    }
}

/// Fact-extraction precision tuning, mirroring `aionforge_consolidate::ExtractionConfig`
/// knob-for-knob (the subject/connective/pronoun gates are compile-time constants, not knobs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtractionSettings {
    /// The confidence floor: a rule whose confidence is below this never fires. Validated
    /// finite and in `[0.0, 1.0]`. Default `0.8` — at or below every shipped typed rule, so
    /// recall is preserved.
    pub min_confidence: f64,
    /// The multiplier applied to a derived fact's inherited episode trust, so a derived fact
    /// ranks strictly below its source episode. Validated finite and in `[0.0, 1.0]`. Default
    /// `0.9`.
    pub derived_trust_factor: f64,
}

impl Default for ExtractionSettings {
    fn default() -> Self {
        // Mirrors `aionforge_consolidate::ExtractionConfig::default()`.
        Self {
            min_confidence: 0.8,
            derived_trust_factor: 0.9,
        }
    }
}

/// Entity-resolution tuning, mirroring `aionforge_consolidate::ResolutionConfig`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResolutionSettings {
    /// How many candidate entities each lexical/vector probe pulls before filtering.
    /// Validated `>= 1`.
    pub candidate_k: usize,
    /// The cosine-distance ceiling under which an embedding neighbor is judged the same
    /// entity (lower is nearer). Validated finite and in `[0.0, 1.0]`.
    pub merge_threshold: f64,
}

impl Default for ResolutionSettings {
    fn default() -> Self {
        // Mirrors `aionforge_consolidate::ResolutionConfig::default()`.
        Self {
            candidate_k: 8,
            merge_threshold: 0.12,
        }
    }
}

/// Supersession/contradiction detection tuning, mirroring
/// `aionforge_consolidate::DetectionConfig` (the predicate-rule map keeps the engine's
/// conservative built-in default and is not config-exposed here).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DetectionSettings {
    /// Whether detection runs at all (off → extraction-only).
    pub enabled: bool,
    /// The incumbent trust at or above which a contradicting new fact is quarantined.
    /// Validated finite and in `[0.0, 1.0]`.
    pub high_trust_threshold: f64,
}

impl Default for DetectionSettings {
    fn default() -> Self {
        // Mirrors `aionforge_consolidate::DetectionConfig::default()` (enabled, 0.7).
        Self {
            enabled: true,
            high_trust_threshold: 0.7,
        }
    }
}

/// Summary-note tuning, mirroring `aionforge_consolidate::SummarizationConfig`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SummarizationSettings {
    /// Whether summarization runs at all (off → extraction/detection only).
    pub enabled: bool,
    /// The fewest facts a subject must have before its cluster is worth summarizing.
    /// Validated `>= 1`.
    pub min_facts: usize,
    /// The fewest distinct entities a cluster must reference before it is summarized.
    /// Validated `>= 1`.
    pub min_entities: usize,
    /// The fraction of a cluster's distinct entities the summary must preserve to clear the
    /// detail-retention guard. Validated finite and in `[0.0, 1.0]`.
    pub entity_retention_threshold: f64,
    /// The mean source-fact confidence a cluster must clear before it is summarized.
    /// Validated finite and in `[0.0, 1.0]`.
    pub confidence_floor: f64,
}

impl Default for SummarizationSettings {
    fn default() -> Self {
        // Mirrors `aionforge_consolidate::SummarizationConfig::default()`.
        Self {
            enabled: true,
            min_facts: 3,
            min_entities: 2,
            entity_retention_threshold: 0.9,
            confidence_floor: 0.6,
        }
    }
}

/// Skill-induction tuning, mirroring `aionforge_consolidate::InductionConfig` (off by
/// default — the binding off-by-default gate).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InductionSettings {
    /// Whether induction runs at all. **Default `false`.**
    pub enabled: bool,
    /// The most recent same-content episodes the recurrence probe counts (also the query
    /// `LIMIT`). Validated `>= 1`.
    pub recurrence_window: usize,
    /// The fewest exact-content recurrences (reuse evidence) before an episode is induced.
    /// Validated `>= 1`.
    pub repetition_threshold: usize,
    /// The fewest distinct whitespace tokens the content must carry to be worth inducing.
    /// Validated `>= 1`.
    pub min_distinct_tokens: usize,
    /// The fewest characters the content must carry to be induced. Validated `>= 1`.
    pub min_body_chars: usize,
    /// The most characters an induced body may carry. Validated `>= min_body_chars`.
    pub max_body_chars: usize,
    /// The prefix on every induced skill name. Validated non-empty.
    pub name_prefix: String,
}

impl Default for InductionSettings {
    fn default() -> Self {
        // Mirrors `aionforge_consolidate::InductionConfig::default()` (off by default).
        Self {
            enabled: false,
            recurrence_window: 50,
            repetition_threshold: 3,
            min_distinct_tokens: 5,
            min_body_chars: 16,
            max_body_chars: 4096,
            name_prefix: "induced/".to_string(),
        }
    }
}

/// A finite `[0.0, 1.0]` threshold check, mirroring [`ForgettingConfig`](crate::ForgettingConfig).
/// The `!(…)` form also rejects a `NaN`, which fails every ordered comparison.
fn validate_unit_interval(key: &str, value: f64) -> Result<(), ConfigError> {
    if !(value.is_finite() && (0.0..=1.0).contains(&value)) {
        return Err(ConfigError::invalid(
            key,
            "must be a finite value in the range [0.0, 1.0]",
        ));
    }
    Ok(())
}

impl ConsolidationConfig {
    /// Check the section's binding invariants, fail-closed with the offending key named.
    ///
    /// `enabled` only controls whether `aionforge serve` owns the background loop. The deterministic
    /// foreground path remains available when that flag is false, so every scheduler/pass knob is
    /// always live and is validated unconditionally. Thresholds must lie in
    /// `[0.0, 1.0]`; durations and counts that would divide-by-zero or wedge a pass must be
    /// positive.
    ///
    /// # Errors
    /// Returns [`ConfigError`] naming the offending key when a threshold is out of range, a
    /// duration or count is zero where it must be positive, or `name_prefix` is empty.
    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        // Scheduler bounds: a zero tick/timeout/lag-ceiling wedges the loop or its budget; a
        // zero batch takes no work.
        for (key, secs) in [
            ("consolidation.tick_interval_secs", self.tick_interval_secs),
            ("consolidation.apply_timeout_secs", self.apply_timeout_secs),
            ("consolidation.lag_ceiling_secs", self.lag_ceiling_secs),
        ] {
            if secs == 0 {
                return Err(ConfigError::invalid(key, "must be greater than zero"));
            }
        }
        if self.batch_size == 0 {
            return Err(ConfigError::invalid(
                "consolidation.batch_size",
                "must be at least 1",
            ));
        }
        // Extraction (both knobs are `[0.0, 1.0]` thresholds).
        validate_unit_interval(
            "consolidation.extraction.min_confidence",
            self.extraction.min_confidence,
        )?;
        validate_unit_interval(
            "consolidation.extraction.derived_trust_factor",
            self.extraction.derived_trust_factor,
        )?;
        // Resolution.
        if self.resolution.candidate_k == 0 {
            return Err(ConfigError::invalid(
                "consolidation.resolution.candidate_k",
                "must be at least 1",
            ));
        }
        validate_unit_interval(
            "consolidation.resolution.merge_threshold",
            self.resolution.merge_threshold,
        )?;
        // Detection.
        validate_unit_interval(
            "consolidation.detection.high_trust_threshold",
            self.detection.high_trust_threshold,
        )?;
        // Summarization.
        if self.summarization.min_facts == 0 {
            return Err(ConfigError::invalid(
                "consolidation.summarization.min_facts",
                "must be at least 1",
            ));
        }
        if self.summarization.min_entities == 0 {
            return Err(ConfigError::invalid(
                "consolidation.summarization.min_entities",
                "must be at least 1",
            ));
        }
        validate_unit_interval(
            "consolidation.summarization.entity_retention_threshold",
            self.summarization.entity_retention_threshold,
        )?;
        validate_unit_interval(
            "consolidation.summarization.confidence_floor",
            self.summarization.confidence_floor,
        )?;
        // Induction (its knobs are validated even when disabled, so a config that later flips
        // `enabled` cannot ship a wedged pass).
        for (key, count) in [
            (
                "consolidation.induction.recurrence_window",
                self.induction.recurrence_window,
            ),
            (
                "consolidation.induction.repetition_threshold",
                self.induction.repetition_threshold,
            ),
            (
                "consolidation.induction.min_distinct_tokens",
                self.induction.min_distinct_tokens,
            ),
            (
                "consolidation.induction.min_body_chars",
                self.induction.min_body_chars,
            ),
        ] {
            if count == 0 {
                return Err(ConfigError::invalid(key, "must be at least 1"));
            }
        }
        if self.induction.max_body_chars < self.induction.min_body_chars {
            return Err(ConfigError::invalid(
                "consolidation.induction.max_body_chars",
                "must be greater than or equal to consolidation.induction.min_body_chars",
            ));
        }
        if self.induction.name_prefix.trim().is_empty() {
            return Err(ConfigError::invalid(
                "consolidation.induction.name_prefix",
                "must be non-empty",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_validate_and_match_the_engine_documented_values() {
        let config = ConsolidationConfig::default();
        config.validate().expect("defaults validate");
        // The exact engine defaults (aionforge-consolidate/src/config.rs); the host mapping
        // test pins the typed equality, this pins the literal primitives.
        assert!(
            !config.enabled,
            "autonomous background consolidation is default-off"
        );
        assert_eq!(config.tick_interval_secs, 5);
        assert_eq!(config.batch_size, 32);
        assert_eq!(config.apply_timeout_secs, 30);
        assert_eq!(config.max_retries, 5);
        assert_eq!(config.lag_ceiling_secs, 5);
        assert_eq!(config.extraction.min_confidence, 0.8);
        assert_eq!(config.extraction.derived_trust_factor, 0.9);
        assert_eq!(config.resolution.candidate_k, 8);
        assert_eq!(config.resolution.merge_threshold, 0.12);
        assert!(config.detection.enabled);
        assert_eq!(config.detection.high_trust_threshold, 0.7);
        assert!(config.summarization.enabled);
        assert_eq!(config.summarization.min_facts, 3);
        assert_eq!(config.summarization.min_entities, 2);
        assert_eq!(config.summarization.entity_retention_threshold, 0.9);
        assert_eq!(config.summarization.confidence_floor, 0.6);
        assert!(!config.induction.enabled, "induction is off by default");
        assert_eq!(config.induction.recurrence_window, 50);
        assert_eq!(config.induction.repetition_threshold, 3);
        assert_eq!(config.induction.min_distinct_tokens, 5);
        assert_eq!(config.induction.min_body_chars, 16);
        assert_eq!(config.induction.max_body_chars, 4096);
        assert_eq!(config.induction.name_prefix, "induced/");
    }

    #[test]
    fn a_consolidation_block_parses_and_round_trips() {
        use figment::{
            Figment,
            providers::{Format, Serialized, Toml},
        };

        // A block that turns induction ON and lowers the summarization floors, layered over
        // the defaults exactly as the real loader does (Serialized::defaults + Toml).
        let toml = r#"
            [consolidation]
            enabled = true
            batch_size = 64

            [consolidation.summarization]
            min_facts = 2
            entity_retention_threshold = 0.8
            confidence_floor = 0.5

            [consolidation.induction]
            enabled = true
            repetition_threshold = 2
            name_prefix = "skill/"
        "#;
        // A tiny wrapper so the section nests under `[consolidation]` like in `Config`.
        #[derive(serde::Serialize, serde::Deserialize, Default)]
        #[serde(default)]
        struct Wrapper {
            consolidation: ConsolidationConfig,
        }
        let parsed: Wrapper = Figment::from(Serialized::defaults(Wrapper::default()))
            .merge(Toml::string(toml))
            .extract()
            .expect("parse");
        let parsed = parsed.consolidation;
        assert!(parsed.enabled, "top-level background loop switch parses");
        assert_eq!(parsed.batch_size, 64);
        assert!(parsed.induction.enabled, "induction was turned on");
        assert_eq!(parsed.induction.repetition_threshold, 2);
        assert_eq!(parsed.induction.name_prefix, "skill/");
        assert_eq!(parsed.summarization.min_facts, 2);
        assert_eq!(parsed.summarization.entity_retention_threshold, 0.8);
        assert_eq!(parsed.summarization.confidence_floor, 0.5);
        // Unset fields fall back to the defaults (serde(default) on every sub-struct).
        assert_eq!(parsed.tick_interval_secs, 5);
        assert_eq!(parsed.resolution.candidate_k, 8);
        assert!(parsed.summarization.enabled);
        parsed.validate().expect("the override block validates");

        // Round-trip through serde so the field names are stable.
        let json = serde_json::to_string(&parsed).expect("serialize");
        let back: ConsolidationConfig = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(back, parsed);
    }

    #[test]
    fn an_absent_block_is_the_default() {
        // An empty document leaves the whole section at its (engine-matching) defaults.
        let parsed: ConsolidationConfig =
            serde_json::from_str("{}").expect("empty object parses via serde(default)");
        assert_eq!(parsed, ConsolidationConfig::default());
    }

    #[test]
    fn an_out_of_range_threshold_is_rejected() {
        for bad in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
            let mut config = ConsolidationConfig::default();
            config.summarization.confidence_floor = bad;
            let error = config
                .validate()
                .expect_err("an out-of-range threshold must be rejected");
            assert!(
                error.to_string().contains("confidence_floor"),
                "the error must name the key, got: {error}"
            );
        }
        let mut config = ConsolidationConfig::default();
        config.detection.high_trust_threshold = 2.0;
        let error = config
            .validate()
            .expect_err("detection threshold is checked too");
        assert!(
            error.to_string().contains("high_trust_threshold"),
            "{error}"
        );

        // The extraction knobs are bounded to [0.0, 1.0] too.
        let mut config = ConsolidationConfig::default();
        config.extraction.min_confidence = 1.5;
        let error = config
            .validate()
            .expect_err("an out-of-range min_confidence is rejected");
        assert!(error.to_string().contains("min_confidence"), "{error}");

        let mut config = ConsolidationConfig::default();
        config.extraction.derived_trust_factor = -0.2;
        let error = config
            .validate()
            .expect_err("an out-of-range derived_trust_factor is rejected");
        assert!(
            error.to_string().contains("derived_trust_factor"),
            "{error}"
        );
    }

    #[test]
    fn a_zero_batch_size_is_rejected() {
        let config = ConsolidationConfig {
            batch_size: 0,
            ..ConsolidationConfig::default()
        };
        let error = config.validate().expect_err("a zero batch takes no work");
        assert!(error.to_string().contains("batch_size"), "{error}");
    }

    #[test]
    fn a_zero_duration_or_count_is_rejected() {
        for mutate in [
            (|c: &mut ConsolidationConfig| c.tick_interval_secs = 0)
                as fn(&mut ConsolidationConfig),
            |c| c.apply_timeout_secs = 0,
            |c| c.lag_ceiling_secs = 0,
            |c| c.resolution.candidate_k = 0,
            |c| c.summarization.min_facts = 0,
            |c| c.induction.recurrence_window = 0,
        ] {
            let mut config = ConsolidationConfig::default();
            mutate(&mut config);
            config
                .validate()
                .expect_err("a zero duration/count is rejected");
        }
    }

    #[test]
    fn an_empty_induced_name_prefix_is_rejected() {
        let mut config = ConsolidationConfig::default();
        config.induction.name_prefix = "   ".to_string();
        let error = config.validate().expect_err("an empty prefix is rejected");
        assert!(error.to_string().contains("name_prefix"), "{error}");
    }
}
