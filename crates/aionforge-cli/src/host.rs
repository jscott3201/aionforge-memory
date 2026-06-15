//! Host-side wiring from layered config into the engine facade.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use aionforge::{
    Authorizer, CaptureConfig, CategoryRule, ConsolidationGuardPolicy, CoreEditPolicy,
    CoreEditRule, DefaultAuthorizer, DriftPolicy, Embedder, EmbedderModel, Embedding,
    ForgettingPolicy, GuardMode, Memory, MemoryConfig, OperatorAwareAuthorizer, PromotionPolicy,
    ReliabilityPolicy, RetrieverConfig, SecurityGate, Store, Timestamp,
};
use aionforge_config::{Config, GuardMode as ConfigGuardMode};
use aionforge_embed::{EmbedError, HttpEmbedder};
use secrecy::SecretString;

use crate::error::CliError;

const STARTUP_EMBEDDER_PROBE: &str = "aionforge-memory startup embedder health check";

#[derive(Debug, Clone)]
pub(crate) struct HostOptions {
    pub(crate) config_path: PathBuf,
    pub(crate) data_dir: Option<PathBuf>,
    pub(crate) deployment: Option<String>,
}

pub(crate) fn load_config(options: &HostOptions) -> Result<Config, CliError> {
    let mut config = Config::from_figment(Config::figment(&options.config_path))?;
    // Resolve the deployment selector with the full three-tier precedence:
    //   flag (`--deployment`)  >  env (`AIONFORGE_ACTIVE_DEPLOYMENT`)  >  file (`active_deployment`).
    // The env-over-file tier is already resolved one layer down by the figment loader
    // (`Config::figment` merges the env provider last, so `config.active_deployment` here is the
    // env value when set, else the file value); this `or_else` only arbitrates the remaining
    // flag-over-(env-or-file) tier. The resolved name is then spliced over the top-level
    // [auth]+[server] posture and re-validated. DEFAULT-OFF (no deployments, no selector) is a
    // no-op that leaves `config` byte-for-byte unchanged.
    let selected = options
        .deployment
        .clone()
        .or_else(|| config.active_deployment.clone());
    config.activate_deployment(selected.as_deref())?;
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
    // DEFAULT-OFF: with `auth.enabled == false` (the default) the memory is built with the
    // [`DefaultAuthorizer`] via `Memory::new`, byte-for-byte today's behavior. When auth is
    // enabled, wrap the default authority with [`OperatorAwareAuthorizer`] so an operator
    // principal (its bit set only by the validated-claims mapper) can surface the `system`
    // namespace at the AND-gated read sites; the operator capability never widens write authority
    // or pre-widens visibility (see the authorizer's own override).
    let memory = if config.auth.enabled {
        let authorizer: Arc<dyn Authorizer> =
            Arc::new(OperatorAwareAuthorizer::new(DefaultAuthorizer));
        Memory::with_authorizer(store, embedder, memory_config, authorizer, &now)?
    } else {
        Memory::new(store, embedder, memory_config, &now)?
    };
    Ok(Arc::new(memory))
}

pub(crate) enum RuntimeEmbedder {
    Http(HttpEmbedder),
    Disabled(DisabledEmbedder),
}

impl RuntimeEmbedder {
    fn is_enabled(&self) -> bool {
        matches!(self, Self::Http(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StartupEmbedderStatus {
    Ready {
        model: EmbedderModel,
        probe_dimension: usize,
    },
    Disabled {
        model: EmbedderModel,
    },
}

pub(crate) async fn check_startup_embedder(
    memory: &Memory<RuntimeEmbedder>,
) -> Result<StartupEmbedderStatus, CliError> {
    let embedder = memory.embedder();
    let model = embedder.model().clone();
    if !embedder.is_enabled() {
        return Ok(StartupEmbedderStatus::Disabled { model });
    }

    let probe = [STARTUP_EMBEDDER_PROBE.to_owned()];
    let mut embeddings = embedder
        .embed(&probe)
        .await
        .map_err(|error| startup_embedder_error(&model, error))?;
    if embeddings.len() != 1 {
        return Err(startup_embedder_error(
            &model,
            format!("expected one probe vector but got {}", embeddings.len()),
        ));
    }
    let embedding = embeddings.pop().expect("length checked");
    let probe_dimension = embedding.dimension();
    if probe_dimension != model.dimension as usize {
        return Err(startup_embedder_error(
            &model,
            format!(
                "probe vector dimension {probe_dimension} does not match configured dimension {}",
                model.dimension
            ),
        ));
    }

    Ok(StartupEmbedderStatus::Ready {
        model,
        probe_dimension,
    })
}

pub(crate) fn render_startup_embedder_status(status: &StartupEmbedderStatus) -> String {
    match status {
        StartupEmbedderStatus::Ready {
            model,
            probe_dimension,
        } => format!(
            "embedder ready {} probe_dimension={} capture_embedding=on",
            model_identity(model),
            probe_dimension
        ),
        StartupEmbedderStatus::Disabled { model } => format!(
            "embedder disabled {} capture_embedding=off",
            model_identity(model)
        ),
    }
}

fn startup_embedder_error(model: &EmbedderModel, error: impl std::fmt::Display) -> CliError {
    CliError::Serve(format!(
        "embedder startup health check failed {}: {error}",
        model_identity(model)
    ))
}

fn model_identity(model: &EmbedderModel) -> String {
    if model.version.trim().is_empty() {
        format!("model={} dimension={}", model.family, model.dimension)
    } else {
        format!(
            "model={} version={} dimension={}",
            model.family, model.version, model.dimension
        )
    }
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
    // Map the layered [consolidation] block into the engine's scheduler config + pass tuning.
    // With an absent block this returns the engine defaults knob-for-knob (no behavior change);
    // the mapping lives in `consolidation_config` so host.rs stays under the file-size cap.
    let (consolidation, pass) = crate::consolidation_config::consolidation_settings(config);
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
        consolidation,
        pass,
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
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    #[test]
    fn open_memory_installs_the_operator_authorizer_only_when_auth_is_enabled() {
        use aionforge_config::IssuerConfig;

        // DEFAULT-OFF: a default config (auth disabled) opens a memory on the DefaultAuthorizer
        // path (`Memory::new`), byte-for-byte today's behavior.
        let mut off = Config::default();
        off.persistence.data_dir = unique_dir("authz-off");
        off.embedder.enabled = false;
        off.embedder.model.clear();
        off.embedder.endpoint.clear();
        let off_dir = off.persistence.data_dir.clone();
        open_memory(&off).expect("default-off memory opens");
        let _ = std::fs::remove_dir_all(off_dir);

        // AUTH ENABLED: an enabled config opens a memory wrapped with OperatorAwareAuthorizer via
        // the `Memory::with_authorizer` seam. Construction succeeds (the wrap is in-process; no
        // token validation runs here — that is the request-path producer's job).
        let mut on = Config::default();
        on.persistence.data_dir = unique_dir("authz-on");
        on.embedder.enabled = false;
        on.embedder.model.clear();
        on.embedder.endpoint.clear();
        on.auth.enabled = true;
        on.auth.issuers = vec![IssuerConfig {
            issuer: "https://dev-7ppqf0duhy7etaet.us.auth0.com/".into(),
            audience: "https://memory.aionforgelabs.com".into(),
            allows_writes: false,
            ..IssuerConfig::default()
        }];
        on.validate().expect("an enabled auth config validates");
        let on_dir = on.persistence.data_dir.clone();
        open_memory(&on).expect("auth-enabled memory opens with the operator authorizer");
        let _ = std::fs::remove_dir_all(on_dir);
    }

    #[tokio::test]
    async fn startup_embedder_check_reports_disabled_without_probe() {
        let dir = unique_dir("startup-disabled");
        let mut config = Config::default();
        config.persistence.data_dir = dir.clone();
        config.embedder.enabled = false;
        config.embedder.model.clear();
        config.embedder.endpoint.clear();

        let memory = open_memory(&config).expect("open memory");
        let status = check_startup_embedder(memory.as_ref())
            .await
            .expect("disabled embedder is reportable");

        assert_eq!(
            status,
            StartupEmbedderStatus::Disabled {
                model: EmbedderModel {
                    family: "disabled".to_owned(),
                    version: String::new(),
                    dimension: config.embedder.dimension,
                },
            }
        );
        assert_eq!(
            render_startup_embedder_status(&status),
            "embedder disabled model=disabled dimension=1536 capture_embedding=off"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn startup_embedder_check_probes_enabled_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    { "index": 0, "embedding": [1.0, 0.0, 0.0, 0.0] },
                ],
                "model": "startup-test",
            })))
            .mount(&server)
            .await;

        let dir = unique_dir("startup-ready");
        let mut config = Config::default();
        config.persistence.data_dir = dir.clone();
        config.embedder.endpoint = format!("{}/v1", server.uri());
        config.embedder.model = "startup-test".to_owned();
        config.embedder.dimension = 4;

        let memory = open_memory(&config).expect("open memory");
        let status = check_startup_embedder(memory.as_ref())
            .await
            .expect("startup probe succeeds");

        assert_eq!(
            render_startup_embedder_status(&status),
            "embedder ready model=startup-test dimension=4 probe_dimension=4 capture_embedding=on"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn startup_embedder_check_refuses_dimension_mismatch() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    { "index": 0, "embedding": [1.0, 0.0] },
                ],
                "model": "startup-test",
            })))
            .mount(&server)
            .await;

        let dir = unique_dir("startup-mismatch");
        let mut config = Config::default();
        config.persistence.data_dir = dir.clone();
        config.embedder.endpoint = format!("{}/v1", server.uri());
        config.embedder.model = "startup-test".to_owned();
        config.embedder.dimension = 4;

        let memory = open_memory(&config).expect("open memory");
        let error = check_startup_embedder(memory.as_ref())
            .await
            .expect_err("dimension mismatch refuses startup");
        let message = error.to_string();

        assert!(message.contains("embedder startup health check failed"));
        assert!(message.contains("model=startup-test dimension=4"));
        assert!(message.contains("embedding dimension 2 does not match the model dimension 4"));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn unique_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "aionforge-cli-host-{label}-{nanos}-{}",
            std::process::id()
        ))
    }

    /// Write a `config.toml` declaring `local` (default-off) and `prod` (enabled, stateless)
    /// deployments into a fresh temp dir, returning the config-file path. The host's
    /// `load_config` reads this file and applies the deployment selector.
    fn write_deployment_config(label: &str) -> (PathBuf, PathBuf) {
        let dir = unique_dir(label);
        std::fs::create_dir_all(&dir).expect("create config dir");
        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            r#"
                [deployments.local.server]
                listen = "127.0.0.1:3918"

                [deployments.prod.server]
                listen = "0.0.0.0:8443"
                stateful = false

                [deployments.prod.auth]
                enabled = true

                [[deployments.prod.auth.issuers]]
                issuer = "https://issuer.example/"
                audience = "https://api.aionforge.dev"
                agent_id_claim = "https://aionforge.dev/agent_id"
            "#,
        )
        .expect("write config file");
        (dir, config_path)
    }

    /// `load_config` for a config-file path with no deployment flag and no active_deployment
    /// key set in the file. Used to prove DEFAULT-OFF round-trips a single-block config.
    fn options_for(config_path: PathBuf, deployment: Option<&str>) -> HostOptions {
        HostOptions {
            config_path,
            data_dir: None,
            deployment: deployment.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn load_config_default_off_leaves_a_single_block_config_unchanged() {
        // A config with no deployments and no selector loads exactly as today: the
        // deployment splice is a no-op and the top-level blocks are the live posture.
        let dir = unique_dir("load-default-off");
        std::fs::create_dir_all(&dir).expect("create dir");
        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            "[server]\nlisten = \"0.0.0.0:8080\"\nstateful = false\n",
        )
        .expect("write");

        let config = load_config(&options_for(config_path, None)).expect("default-off loads");
        assert_eq!(config.server.listen.to_string(), "0.0.0.0:8080");
        assert!(!config.server.stateful);
        assert!(!config.auth.enabled, "no auth without a deployment");
        assert!(config.deployments.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_deployment_flag_selects_and_splices_the_named_blocks() {
        let (dir, config_path) = write_deployment_config("load-flag-select");
        let config =
            load_config(&options_for(config_path, Some("prod"))).expect("prod selected by flag");
        assert!(config.auth.enabled, "the prod auth block is now live");
        assert!(!config.server.stateful, "the prod server block is now live");
        assert_eq!(config.server.listen.to_string(), "0.0.0.0:8443");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_deployment_flag_overrides_the_active_deployment_key() {
        // The config file pins active_deployment = "prod"; the --deployment flag overrides it
        // to the default-off "local" profile, proving flag precedence over the config key.
        let dir = unique_dir("load-flag-overrides-key");
        std::fs::create_dir_all(&dir).expect("create dir");
        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            r#"
                active_deployment = "prod"

                [deployments.local.server]
                listen = "127.0.0.1:3918"

                [deployments.prod.server]
                listen = "0.0.0.0:8443"
                stateful = false

                [deployments.prod.auth]
                enabled = true

                [[deployments.prod.auth.issuers]]
                issuer = "https://issuer.example/"
                audience = "https://api.aionforge.dev"
                agent_id_claim = "https://aionforge.dev/agent_id"
            "#,
        )
        .expect("write");

        let config = load_config(&options_for(config_path, Some("local")))
            .expect("the flag selects local over the prod key");
        assert!(
            !config.auth.enabled,
            "the flag selected the default-off local profile, not prod"
        );
        assert_eq!(
            config.server.listen.to_string(),
            "127.0.0.1:3918",
            "local's server block is live"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn declared_deployments_with_no_selection_fail_to_load() {
        // No flag, no active_deployment key, but deployments are declared: load fails closed.
        let (dir, config_path) = write_deployment_config("load-fail-closed");
        let err = load_config(&options_for(config_path, None)).expect_err("a choice is required");
        assert!(
            matches!(err, CliError::Config(ref boxed)
                if matches!(boxed.as_ref(), aionforge_config::ConfigError::Missing(key) if key == "active_deployment")),
            "the missing-selector error surfaces through the CLI error: {err}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
