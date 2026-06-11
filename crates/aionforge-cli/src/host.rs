//! Host-side wiring from layered config into the engine facade.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use aionforge::{
    CaptureConfig, CategoryRule, ConsolidationGuardPolicy, CoreEditPolicy, CoreEditRule,
    DriftPolicy, Embedder, EmbedderModel, Embedding, ForgettingPolicy, GuardMode, Memory,
    MemoryConfig, PromotionPolicy, ReliabilityPolicy, RetrieverConfig, SecurityGate, Store,
    Timestamp,
};
use aionforge_config::{Config, GuardMode as ConfigGuardMode};
use aionforge_embed::{EmbedError, HttpEmbedder};
use secrecy::SecretString;

use crate::error::CliError;

#[derive(Debug, Clone)]
pub(crate) struct HostOptions {
    pub(crate) config_path: PathBuf,
    pub(crate) data_dir: Option<PathBuf>,
}

pub(crate) fn load_config(options: &HostOptions) -> Result<Config, CliError> {
    let mut config = Config::from_figment(Config::figment(&options.config_path))?;
    if let Some(data_dir) = &options.data_dir {
        config.persistence.data_dir = data_dir.clone();
        config.validate()?;
    }
    Ok(config)
}

pub(crate) fn open_memory(config: &Config) -> Result<Arc<Memory<RuntimeEmbedder>>, CliError> {
    let now = Timestamp::now();
    let memory_config = memory_config(config)?;
    let embedder = runtime_embedder(config)?;
    let store = Arc::new(Store::open_or_recover(
        config.data_dir(),
        config.store_config(),
        &now,
    )?);
    Ok(Arc::new(Memory::new(store, embedder, memory_config, &now)?))
}

pub(crate) enum RuntimeEmbedder {
    Http(HttpEmbedder),
    Disabled(DisabledEmbedder),
}

pub(crate) fn runtime_embedder(config: &Config) -> Result<RuntimeEmbedder, CliError> {
    if config.embedder.enabled {
        return Ok(RuntimeEmbedder::Http(HttpEmbedder::from_config(
            &config.embedder,
            config.resolve_api_key_from_env()?,
        )?));
    }

    Ok(RuntimeEmbedder::Disabled(DisabledEmbedder {
        identity: EmbedderModel {
            family: disabled_family(&config.embedder.model),
            version: String::new(),
            dimension: config.embedder.dimension,
        },
    }))
}

pub(crate) fn memory_config(config: &Config) -> Result<MemoryConfig, CliError> {
    let audit_seed = if config.security.sign_audit_events {
        config.resolve_audit_seed_from_env()?
    } else {
        None
    };
    Ok(MemoryConfig {
        capture: CaptureConfig {
            embed_on_capture: config.embedder.enabled,
            ..CaptureConfig::default()
        },
        retriever: retriever_config(config),
        security: security_gate(config, audit_seed),
        promotion: promotion_policy(config),
        reliability: reliability_policy(config),
        forgetting: forgetting_policy(config),
        drift: drift_policy(config),
        erasure: Default::default(),
        core_block: core_block_policy(config),
        consolidation_guard: consolidation_guard_policy(config),
    })
}

fn retriever_config(config: &Config) -> RetrieverConfig {
    RetrieverConfig {
        default_fanout: config.retrieval.default_k as usize,
        decay_enabled: config.decay.enabled,
        episodic_half_life_secs: config.decay.episodic_half_life_secs as f64,
        semantic_half_life_secs: config.decay.semantic_half_life_secs as f64,
        cooling_enabled: config.drift.enabled,
        cooling_factor: config.drift.cooling_factor,
        ..RetrieverConfig::default()
    }
}

fn security_gate(config: &Config, audit_seed: Option<SecretString>) -> SecurityGate {
    SecurityGate {
        signed_writes: config.security.signed_writes,
        clock_skew_tolerance_ms: config.security.clock_skew_tolerance_ms,
        sign_audit_events: config.security.sign_audit_events,
        audit_data_dir: config
            .security
            .sign_audit_events
            .then(|| config.data_dir().to_path_buf()),
        audit_seed,
    }
}

fn promotion_policy(config: &Config) -> PromotionPolicy {
    PromotionPolicy {
        enabled: config.promotion.enabled,
        default_k: config.promotion.default_k,
        default_threshold: config.promotion.default_threshold,
        prior_alpha: config.promotion.prior_alpha,
        prior_beta: config.promotion.prior_beta,
        default_category: config.promotion.default_category.clone(),
        categories: config
            .promotion
            .categories
            .iter()
            .map(|(category, rule)| {
                (
                    category.clone(),
                    CategoryRule {
                        k: rule.k,
                        threshold: rule.threshold,
                    },
                )
            })
            .collect(),
    }
}

fn reliability_policy(config: &Config) -> ReliabilityPolicy {
    ReliabilityPolicy {
        enabled: config.reliability.enabled,
        prior_alpha: config.reliability.prior_alpha,
        prior_beta: config.reliability.prior_beta,
        default_category: config.reliability.default_category.clone(),
        w_contradict: config.reliability.w_contradict,
        w_attest_invalid: config.reliability.w_attest_invalid,
        w_agree: config.reliability.w_agree,
    }
}

fn forgetting_policy(config: &Config) -> ForgettingPolicy {
    ForgettingPolicy {
        enabled: config.forgetting.enabled,
        importance_floor: config.forgetting.importance_floor,
        trust_floor: config.forgetting.trust_floor,
        min_age_secs: config.forgetting.min_age_secs,
        batch_cap: config.forgetting.batch_cap,
        forget_bad_patterns: config.forgetting.forget_bad_patterns,
        episodic_half_life_secs: config.decay.episodic_half_life_secs as f64,
        semantic_half_life_secs: config.decay.semantic_half_life_secs as f64,
    }
}

fn drift_policy(config: &Config) -> DriftPolicy {
    DriftPolicy {
        enabled: config.drift.enabled,
        drift_threshold: config.drift.drift_threshold,
        behavior_window_secs: config.drift.behavior_window_secs,
        behavior_sample_size: config.drift.behavior_sample_size,
        min_sample_size: config.drift.min_sample_size,
        cooling_window_secs: config.drift.cooling_window_secs,
        cooling_factor: config.drift.cooling_factor,
        core_proximity_threshold: config.drift.core_proximity_threshold,
        high_trust_threshold: config.drift.high_trust_threshold,
    }
}

fn core_block_policy(config: &Config) -> CoreEditPolicy {
    CoreEditPolicy {
        default_rule: core_edit_rule(config.core_block.default_rule),
        redline_requires_human: config.core_block.redline_requires_human,
        rules: config
            .core_block
            .rules
            .iter()
            .map(|(sensitivity, rule)| (sensitivity.clone(), core_edit_rule(*rule)))
            .collect::<BTreeMap<_, _>>(),
        human_attester_ids: config.core_block.human_attester_ids.clone(),
    }
}

fn core_edit_rule(rule: aionforge_config::CoreEditRuleConfig) -> CoreEditRule {
    CoreEditRule {
        k: rule.k,
        require_human: rule.require_human,
    }
}

fn consolidation_guard_policy(config: &Config) -> ConsolidationGuardPolicy {
    ConsolidationGuardPolicy {
        mode: match config.consolidation_guard.mode {
            ConfigGuardMode::Refuse => GuardMode::Refuse,
            ConfigGuardMode::Warn => GuardMode::Warn,
        },
        declared_consolidator_family: config
            .consolidation_guard
            .declared_consolidator_family
            .clone(),
    }
}

fn disabled_family(configured: &str) -> String {
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        "disabled".to_owned()
    } else {
        trimmed.to_owned()
    }
}

pub(crate) struct DisabledEmbedder {
    identity: EmbedderModel,
}

impl Embedder for RuntimeEmbedder {
    type Error = EmbedError;

    async fn embed(&self, inputs: &[String]) -> Result<Vec<Embedding>, Self::Error> {
        match self {
            Self::Http(embedder) => embedder.embed(inputs).await,
            Self::Disabled(embedder) => embedder.embed(inputs).await,
        }
    }

    fn model(&self) -> &EmbedderModel {
        match self {
            Self::Http(embedder) => embedder.model(),
            Self::Disabled(embedder) => embedder.model(),
        }
    }
}

impl Embedder for DisabledEmbedder {
    type Error = EmbedError;

    async fn embed(&self, inputs: &[String]) -> Result<Vec<Embedding>, Self::Error> {
        if inputs.is_empty() {
            Ok(Vec::new())
        } else {
            Err(EmbedError::Unavailable(
                "embedding is disabled by configuration".to_owned(),
            ))
        }
    }

    fn model(&self) -> &EmbedderModel {
        &self.identity
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionforge_config::{CategoryPromotionRule, CoreEditRuleConfig};

    #[test]
    fn maps_layered_config_into_engine_policy() {
        let mut config = Config::default();
        config.retrieval.default_k = 37;
        config.decay.enabled = true;
        config.decay.episodic_half_life_secs = 11;
        config.decay.semantic_half_life_secs = 22;
        config.security.signed_writes = true;
        config.security.sign_audit_events = true;
        config.persistence.data_dir = PathBuf::from("/tmp/aionforge-cli-host");
        config.promotion.enabled = true;
        config.promotion.categories.insert(
            "pii".to_owned(),
            CategoryPromotionRule {
                k: 4,
                threshold: 0.80,
            },
        );
        config.core_block.rules.insert(
            "pii".to_owned(),
            CoreEditRuleConfig {
                k: 2,
                require_human: false,
            },
        );

        let mapped = memory_config(&config).expect("maps");

        assert_eq!(mapped.retriever.default_fanout, 37);
        assert!(mapped.retriever.decay_enabled);
        assert_eq!(mapped.forgetting.episodic_half_life_secs, 11.0);
        assert!(mapped.security.signed_writes);
        assert_eq!(
            mapped.security.audit_data_dir,
            Some(config.data_dir().to_path_buf())
        );
        assert_eq!(mapped.promotion.categories["pii"].k, 4);
        assert_eq!(mapped.core_block.rules["pii"].k, 2);
    }

    #[test]
    fn disabled_audit_signing_does_not_require_the_named_seed_env() {
        let mut config = Config::default();
        config.security.sign_audit_events = false;
        config.security.audit_key_env = Some("AIONFORGE_TEST_MISSING_AUDIT_SEED".to_owned());

        let mapped = memory_config(&config).expect("disabled signing ignores seed env");

        assert!(!mapped.security.sign_audit_events);
        assert!(mapped.security.audit_seed.is_none());
    }
}
