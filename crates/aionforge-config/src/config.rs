//! The configuration tree and its defaults, validation, and derived views.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use aionforge_store::{DEFAULT_EMBEDDING_DIMENSION, StoreConfig, default_data_dir};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::auth::AuthConfig;
use crate::core_block::CoreBlockConfig;
use crate::deployment::DeploymentConfig;
use crate::drift::DriftConfig;
use crate::error::ConfigError;
use crate::forgetting::ForgettingConfig;
use crate::guard::ConsolidationGuardConfig;
use crate::server::ServerHttpConfig;

/// The largest sane per-request embedder timeout (ten minutes). A larger value is almost
/// certainly a units mistake (seconds typed as milliseconds), so it is rejected rather than
/// hanging a capture or recall on a wedged endpoint.
const MAX_EMBEDDER_TIMEOUT_MS: u64 = 600_000;

/// The widest sane clock-skew tolerance for signed writes (five minutes, 06 §3). The window
/// bounds replay/storm exposure, so a misconfigured production deployment cannot effectively
/// disable replay protection by setting it arbitrarily high; five minutes covers normal NTP
/// drift across hosts and time zones with margin.
const MAX_CLOCK_SKEW_TOLERANCE_MS: u64 = 300_000;

/// The whole Aionforge configuration, assembled from defaults, a TOML file, the
/// environment, and caller flags (see [`crate`] for precedence).
///
/// Every field has a default, so an empty file and an unset environment still yield a
/// usable config. No field holds a secret value: the embedder names the environment
/// variable that holds its API key rather than carrying the key, so logging a `Config`
/// never leaks one.
///
/// `Config` derives `PartialEq` but not `Eq`: [`PromotionConfig`] and [`ReliabilityConfig`]
/// carry real-valued thresholds, weights, and Beta priors (`f64`), which are not `Eq`. Nothing
/// depends on a total equality over a config, so the honest real-valued model is kept rather
/// than encoding the probabilities as integers.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// On-disk state: where the WAL, snapshots, and logs live.
    pub persistence: PersistenceConfig,
    /// The embedding/inference endpoint and the model identity it serves.
    pub embedder: EmbedderConfig,
    /// Retrieval defaults applied when a query does not override them.
    pub retrieval: RetrievalConfig,
    /// Security posture toggles.
    pub security: SecurityConfig,
    /// Quorum-promotion posture: the attestation count, posterior threshold, and Beta
    /// priors that gate a team fact's promotion to global (06 §4). Off by default.
    pub promotion: PromotionConfig,
    /// Trust-scoring posture: the Beta prior and the asymmetric agreement/decay weights that
    /// move an agent's reliability as its facts are corroborated or invalidated (06 §5). Off
    /// by default.
    pub reliability: ReliabilityConfig,
    /// Importance-decay posture: the per-tier half-lives that sink a memory's effective
    /// importance with elapsed time since last access (05 §2). Off by default.
    pub decay: DecayConfig,
    /// Active-forgetting posture: the floors and guards for the soft-forget sweep
    /// (05 §2). Off by default.
    pub forgetting: ForgettingConfig,
    /// Core-block edit strictness (05 §4): the second-attester requirement for
    /// identity-tier edits. **Always on** — unlike its siblings there is no master
    /// switch, only strictness; the all-default posture is the spec's floor (one
    /// non-editor attester), not an off state.
    pub core_block: CoreBlockConfig,
    /// Drift-detection posture (05 §1): the off-cursor sweep's behavior-sample
    /// bounds and warning threshold, and the cooling window for core-proximate
    /// facts. Off by default.
    pub drift: DriftConfig,
    /// Cross-family consolidation-guard posture (07 §3): refuse-or-warn mode and
    /// the declared consolidating family the startup single-family check reads.
    /// Always-on policy, inert until an inference-backed consolidation rule runs.
    pub consolidation_guard: ConsolidationGuardConfig,
    /// OAuth resource-server posture: the master switch and trusted token issuers.
    /// **Default-off** — when [`AuthConfig::enabled`] is `false` (the default) the
    /// server derives no identity from a connection. Config only in this PR; JWT
    /// validation and principal mapping read these fields in later work.
    pub auth: AuthConfig,
    /// Streamable HTTP server posture: the bind address, the Host/Origin allow-lists,
    /// and the stateful-session flag (fork#6). **No master switch** — an HTTP server
    /// always binds somewhere, so the all-default posture (loopback `127.0.0.1:3918`,
    /// stateful, no explicit allow-lists) is today's behavior. CLI `serve http` flags
    /// override these fields field-for-field when present.
    pub server: ServerHttpConfig,
    /// Named per-deployment `[auth]`+`[server]` profiles (PR6b), keyed by name in canonical
    /// (`BTreeMap`) order. Empty by default, which is today's single-profile config exactly.
    /// When non-empty, [`Config::activate_deployment`] selects one by name to splice over the
    /// top-level [`Config::auth`]/[`Config::server`] blocks; every declared profile — active or
    /// not — is validated by [`Config::validate`], so a broken inactive profile fails at load.
    pub deployments: BTreeMap<String, DeploymentConfig>,
    /// The deployment selected at load when no CLI `--deployment` flag is given. `None` (the
    /// default) selects nothing: with an empty [`Config::deployments`] map this is DEFAULT-OFF
    /// (the top-level blocks run unchanged); with declared deployments it is a fail-closed
    /// error, forcing an explicit choice. Maps from `AIONFORGE_ACTIVE_DEPLOYMENT` for free via
    /// the env layer's `__`-split provider.
    pub active_deployment: Option<String>,
}

/// On-disk state configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistenceConfig {
    /// The user-space root for the WAL, snapshots, and logs. Defaults to `~/.aionforge`.
    pub data_dir: PathBuf,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
        }
    }
}

/// Embedding/inference endpoint configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbedderConfig {
    /// Whether embedding is on. When off, retrieval falls back to lexical and graph
    /// signals and capture defers embedding to consolidation.
    pub enabled: bool,
    /// The OpenAI-compatible base URL. `https://` is required unless the host is
    /// localhost.
    pub endpoint: String,
    /// The model identity recorded on every embedding for the cross-family guard.
    pub model: String,
    /// The embedding dimension. Binding (data-model §13.5): every vector index is built
    /// at this dimension and a change is a migration.
    pub dimension: u32,
    /// The **name** of the environment variable that holds the endpoint's API key, or
    /// none for an unauthenticated (e.g. local) endpoint. The key itself never lives in
    /// the config; see [`Config::resolve_api_key`].
    pub api_key_env: Option<String>,
    /// Per-request timeout, in milliseconds.
    pub timeout_ms: u64,
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            endpoint: "http://127.0.0.1:1234/v1".to_owned(),
            model: "codestral-embed-2505".to_owned(),
            dimension: DEFAULT_EMBEDDING_DIMENSION,
            api_key_env: None,
            timeout_ms: 30_000,
        }
    }
}

/// Retrieval defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetrievalConfig {
    /// Default number of results when a query does not specify one.
    pub default_k: u32,
    /// The reciprocal-rank-fusion constant (the conventional default is 60).
    pub fusion_constant: u32,
    /// The default retrieval mode label (e.g. `hybrid`, `lexical`, `vector`).
    pub default_mode: String,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            default_k: 12,
            fusion_constant: 60,
            default_mode: "hybrid".to_owned(),
        }
    }
}

/// Security posture toggles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    /// Whether writes must be signed. Off by default, on for production (§8.4).
    pub signed_writes: bool,
    /// The clock-skew tolerance in milliseconds for signed verifications (06 §3): a
    /// signature whose timestamp deviates from the substrate clock by more than this is
    /// rejected (replay/storm mitigation). Consulted by signed writes and promotion when
    /// they are on, and **always** by the core-block edit gate (05 §4, which has no off
    /// switch) — so it is validated unconditionally, bounded to
    /// `MAX_CLOCK_SKEW_TOLERANCE_MS`.
    pub clock_skew_tolerance_ms: u64,
    /// Whether capture-side redaction of configured patterns is on. **Reserved — not yet
    /// consulted.** The engine builds the capture filter with `CaptureFilter::with_defaults`
    /// unconditionally, so the conservative v1.0 redaction + injection-marker set always runs
    /// and setting this `false` today does **not** disable it. The field is kept with a `true`
    /// default so a future host-supplied filter toggle (or a diagnostic "filter off" mode) can
    /// honor it without forcing a config migration. The injection-marker hardening and its
    /// per-marker hit metrics (`capture_injection_marker_hits_total`, M6.T03) ride on that same
    /// always-on filter.
    pub redaction: bool,
    /// Whether the substrate signs the audit events it authors (06 §6). Off by default:
    /// doing nothing leaves every audit event unsigned, exactly as before, with zero setup.
    /// Turning it on auto-mints a self-managed substrate keypair on first run and stamps a
    /// signature on every substrate-authored audit event, so the audit subgraph becomes
    /// publicly verifiable. The verifier ships in the same change, so an enabled signature
    /// always means something — there is no signed-but-unverifiable state.
    pub sign_audit_events: bool,
    /// The **name** of the environment variable holding a base64-encoded 32-byte Ed25519 seed,
    /// or none to self-custody a seed file under the data directory (the default).
    ///
    /// This is the opt-in custody escalation, mirroring [`EmbedderConfig::api_key_env`]: name a
    /// variable here to delegate the seed to an operator's secret manager instead of the on-disk
    /// file. The seed itself never lives in the config; see [`Config::resolve_audit_seed`].
    pub audit_key_env: Option<String>,
    /// Reserved for cross-instance audit-key pinning (v2 federation, 06 line 5). **Not yet
    /// consulted**: v1 anchors trust solely in the substrate's own out-of-band keyring file, so a
    /// single-host deployment needs no external pins. The field name is reserved with an empty
    /// default so adding federation later does not force a config migration. Setting it today has
    /// no effect.
    pub trusted_audit_keys: Vec<String>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            signed_writes: false,
            clock_skew_tolerance_ms: 60_000,
            redaction: true,
            sign_audit_events: false,
            audit_key_env: None,
            trusted_audit_keys: Vec::new(),
        }
    }
}

/// Quorum-promotion posture (06 §4): when a team fact accumulates enough independent,
/// signed attestations and a high enough reliability-weighted posterior, it promotes to
/// the `global` namespace.
///
/// Off by default — a single-team or development deployment never promotes, with no
/// overhead. Turning it on is a deliberate production decision. The `k` count and the
/// posterior `threshold` are the two gates (both must clear); sensitive categories raise
/// both via [`categories`](PromotionConfig::categories). The `prior_*` fields seed the
/// Beta posterior over "this fact is correct"; the default Beta(1, 1) is uninformative.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PromotionConfig {
    /// Whether quorum promotion runs at all. Off by default; when off, attestations are
    /// still recorded but no fact is ever promoted.
    pub enabled: bool,
    /// The number of distinct, independent attesters required before a candidate is even
    /// considered. A quorum of one is not a quorum, so this is validated `>= 2`.
    pub default_k: u64,
    /// The posterior bar a candidate must clear to promote, in `(0.5, 1.0]`. At or below
    /// `0.5` the uninformative prior alone could clear it; above `1.0` is unreachable.
    pub default_threshold: f64,
    /// The Beta prior's `alpha` (pseudo-count of correctness) over a candidate. Default
    /// `1.0`. Validated `> 0`.
    pub prior_alpha: f64,
    /// The Beta prior's `beta` (pseudo-count of incorrectness). Default `1.0`. Validated
    /// `> 0`.
    pub prior_beta: f64,
    /// The category bucket an attestation with no explicit category falls into, used for
    /// the per-category `k`/threshold lookup and the per-attester reliability read.
    pub default_category: String,
    /// Per-category overrides. A sensitive category (e.g. `pii`) raises `k` and the
    /// threshold above the defaults. Empty by default; a `BTreeMap` keeps the rendered key
    /// order canonical. When a candidate's attestations span several categories the effective
    /// gate composes the maximum `k` and the maximum threshold independently, so the bar may be
    /// stricter than any single rule and a sensitive-category fact is never promoted under a
    /// laxer count or threshold.
    pub categories: BTreeMap<String, CategoryPromotionRule>,
}

impl Default for PromotionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_k: 3,
            // The highest threshold a quorum of `default_k = 3` can reach under the Beta(1, 1)
            // prior: the bounded posterior tops out at (alpha + k) / (alpha + beta + k) = 4/5, so a
            // higher default would be mutually unsatisfiable with k = 3 and promote nothing.
            // `Config::validate` enforces this reachability; a stricter global bar raises both.
            default_threshold: 0.80,
            prior_alpha: 1.0,
            prior_beta: 1.0,
            default_category: "reliability".to_owned(),
            categories: BTreeMap::new(),
        }
    }
}

/// A per-category promotion override (06 §4): a stricter `k` and posterior `threshold`
/// for a named trust category.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CategoryPromotionRule {
    /// The required distinct-attester count for this category. Validated `>= 2`.
    pub k: u64,
    /// The posterior bar for this category, in `(0.5, 1.0]`.
    pub threshold: f64,
}

/// Trust-scoring posture (06 §5): how an agent's per-category reliability moves as the facts
/// it produced or attested are later corroborated or invalidated.
///
/// Off by default — a deployment that does not score reliability records no
/// `ReliabilityUpdate` events and leaves every agent at its neutral prior, with no overhead.
/// When on, reliability is **doubly-derived state**: the canonical record is an append-only
/// multiset of `ReliabilityUpdate` audit events, and `Agent.trust_scores` (plus `Fact.stats.trust`)
/// are recomputable caches folded from it. These knobs set the Beta prior every agent category
/// starts at and the three event weights.
///
/// The weights are deliberately **asymmetric**: an agreement gain is smaller than a decay, so
/// reliability is slow to farm and quick to lose. The guard `w_agree < w_contradict` makes
/// that concrete — a producer can never earn back, through agreement, as much as one of its
/// own contradicted facts costs it. (The attester channel is loss-only, so it needs no such
/// guard.) `prior_*` seed the Beta over "this agent is reliable"; the default Beta(1, 1) is
/// uninformative.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReliabilityConfig {
    /// Whether trust scoring runs at all. Off by default; when off, no reliability event is
    /// recorded and every agent's trust stays at the prior.
    pub enabled: bool,
    /// The Beta prior's `alpha` (pseudo-count of reliable outcomes) every agent category
    /// starts at. Default `1.0`. Validated finite `> 0`.
    pub prior_alpha: f64,
    /// The Beta prior's `beta` (pseudo-count of unreliable outcomes). Default `1.0`. Validated
    /// finite `> 0`.
    pub prior_beta: f64,
    /// The trust category an update with no explicit category falls into, mirroring
    /// [`PromotionConfig::default_category`] so the two subsystems bucket by the same name.
    /// Validated non-empty when enabled.
    pub default_category: String,
    /// The decay a **producing** agent takes when one of its facts is contradicted and
    /// quarantined (added to the category's Beta `beta`). Default `1.0`. Validated finite `>= 0`.
    pub w_contradict: f64,
    /// The decay an **attesting** agent takes when a fact it attested is later invalidated
    /// (added to `beta`). Default `1.0`. Validated finite `>= 0`.
    pub w_attest_invalid: f64,
    /// The gain a **producing** agent earns when a later, distinct-authored canonical fact
    /// corroborates its assertion (added to `alpha`). Default `0.25`, deliberately smaller than
    /// the decay weights. Validated finite `>= 0` and strictly less than `w_contradict`. Set to
    /// `0.0` to ship a decay-only posture.
    pub w_agree: f64,
}

impl Default for ReliabilityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            prior_alpha: 1.0,
            prior_beta: 1.0,
            default_category: "reliability".to_owned(),
            w_contradict: 1.0,
            w_attest_invalid: 1.0,
            w_agree: 0.25,
        }
    }
}

/// Importance-decay configuration (05 §2, M5.T01).
///
/// Decay is a pure read-time computation — the substrate never writes a decayed value back
/// (§13.7) — so this configures only *whether* elapsed time sinks effective importance and
/// how fast per tier. The defaults are deliberately conservative (a half-life of days for
/// episodic memory, a year for semantic), because aggressive forgetting risks losing
/// rarely-but-critically-needed facts; pinned memories ignore decay entirely.
///
/// The host maps these knobs into the retrieval crate's `RetrieverConfig` — the
/// half-lives carried as `f64` seconds — the same host-side indirection as
/// [`ReliabilityConfig`] into the engine's reliability policy, so neither the engine nor
/// the retrieval crate takes a config dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DecayConfig {
    /// Whether elapsed time decays effective importance at all. Off by default; when off,
    /// rankings read the stored write-time importance unchanged.
    pub enabled: bool,
    /// Half-life for session-scoped episodic memory, in seconds. Default seven days.
    /// Validated non-zero when enabled.
    pub episodic_half_life_secs: u64,
    /// Half-life for semantic and identity memory, in seconds. Default 365 days.
    /// Validated non-zero when enabled.
    pub semantic_half_life_secs: u64,
}

impl Default for DecayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            episodic_half_life_secs: 604_800,
            semantic_half_life_secs: 31_536_000,
        }
    }
}

impl Config {
    /// The store's binding configuration, derived from the embedder dimension.
    ///
    /// This is the "absorb" relationship: the embedder dimension is the single source of
    /// truth, and the store is created at that dimension, so the §13.5 check lines up.
    #[must_use]
    pub fn store_config(&self) -> StoreConfig {
        StoreConfig {
            embedding_dimension: self.embedder.dimension,
        }
    }

    /// The on-disk data directory.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.persistence.data_dir
    }

    /// Resolve the embedder's API key by reading the environment variable named in
    /// `embedder.api_key_env`, through the supplied lookup.
    ///
    /// Returns `Ok(None)` when no variable is named (an unauthenticated endpoint). The
    /// returned [`SecretString`] redacts in logs and zeroizes on drop, and the key is
    /// never stored on the [`Config`].
    ///
    /// # Errors
    /// Returns [`ConfigError::SecretEnvMissing`] (naming the variable, never a value)
    /// when a variable is named but unset.
    pub fn resolve_api_key<F>(&self, lookup: F) -> Result<Option<SecretString>, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        match &self.embedder.api_key_env {
            None => Ok(None),
            Some(name) => match lookup(name) {
                Some(value) => Ok(Some(SecretString::from(value))),
                None => Err(ConfigError::SecretEnvMissing(name.clone())),
            },
        }
    }

    /// Resolve the embedder's API key from the process environment.
    ///
    /// # Errors
    /// See [`Config::resolve_api_key`].
    pub fn resolve_api_key_from_env(&self) -> Result<Option<SecretString>, ConfigError> {
        self.resolve_api_key(|name| std::env::var(name).ok())
    }

    /// Resolve the substrate audit-signing seed by reading the environment variable named in
    /// `security.audit_key_env`, through the supplied lookup. Mirrors [`Config::resolve_api_key`]
    /// for the opt-in audit-key custody escalation.
    ///
    /// The variable holds a base64-encoded 32-byte Ed25519 seed, which is **not** decoded here —
    /// the trust layer owns that. Returns `Ok(None)` when no variable is named, the common case
    /// where the substrate self-custodies a seed file under the data directory instead. The
    /// returned [`SecretString`] redacts in logs and zeroizes on drop, and the seed never lives on
    /// the [`Config`].
    ///
    /// # Errors
    /// Returns [`ConfigError::SecretEnvMissing`] (naming the variable, never a value) when a
    /// variable is named but unset.
    pub fn resolve_audit_seed<F>(&self, lookup: F) -> Result<Option<SecretString>, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        match &self.security.audit_key_env {
            None => Ok(None),
            Some(name) => match lookup(name) {
                Some(value) => Ok(Some(SecretString::from(value))),
                None => Err(ConfigError::SecretEnvMissing(name.clone())),
            },
        }
    }

    /// Resolve the substrate audit-signing seed from the process environment.
    ///
    /// # Errors
    /// See [`Config::resolve_audit_seed`].
    pub fn resolve_audit_seed_from_env(&self) -> Result<Option<SecretString>, ConfigError> {
        self.resolve_audit_seed(|name| std::env::var(name).ok())
    }

    /// Check every binding invariant, returning the first violation with a clear,
    /// secret-free message.
    ///
    /// # Errors
    /// Returns [`ConfigError`] naming the offending key when a required value is missing
    /// or a value is out of range.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.persistence.data_dir.as_os_str().is_empty() {
            return Err(ConfigError::missing("persistence.data_dir"));
        }
        if self.embedder.dimension == 0 {
            return Err(ConfigError::invalid(
                "embedder.dimension",
                "must be greater than zero",
            ));
        }
        if self.embedder.enabled {
            if self.embedder.endpoint.trim().is_empty() {
                return Err(ConfigError::missing("embedder.endpoint"));
            }
            if self.embedder.model.trim().is_empty() {
                return Err(ConfigError::missing("embedder.model"));
            }
            if !endpoint_transport_is_allowed(&self.embedder.endpoint) {
                return Err(ConfigError::invalid(
                    "embedder.endpoint",
                    "must use https:// unless the host is localhost",
                ));
            }
            if self.embedder.timeout_ms == 0 {
                return Err(ConfigError::invalid(
                    "embedder.timeout_ms",
                    "must be greater than zero",
                ));
            }
            if self.embedder.timeout_ms > MAX_EMBEDDER_TIMEOUT_MS {
                return Err(ConfigError::invalid(
                    "embedder.timeout_ms",
                    "must be at most 600000 (ten minutes)",
                ));
            }
        }
        if self.retrieval.default_k == 0 {
            return Err(ConfigError::invalid(
                "retrieval.default_k",
                "must be greater than zero",
            ));
        }
        if self.retrieval.fusion_constant == 0 {
            return Err(ConfigError::invalid(
                "retrieval.fusion_constant",
                "must be greater than zero",
            ));
        }
        // The skew window is validated unconditionally: signed writes and promotion gate
        // on it only when enabled, but the core-block edit gate (05 §4) is always on and
        // always consumes it — a zero window would silently refuse every identity edit
        // (skew is always >= 0), so it is a configuration error, not a silent lockout.
        if self.security.clock_skew_tolerance_ms == 0 {
            return Err(ConfigError::invalid(
                "security.clock_skew_tolerance_ms",
                "must be greater than zero (the always-on core-block edit gate consumes it)",
            ));
        }
        if self.security.clock_skew_tolerance_ms > MAX_CLOCK_SKEW_TOLERANCE_MS {
            return Err(ConfigError::invalid(
                "security.clock_skew_tolerance_ms",
                "must be at most 300000 (five minutes)",
            ));
        }
        if self.promotion.enabled {
            // Priors first: the reachability check inside `validate_promotion_rule` divides by
            // them, so they must be finite and positive before any gate is evaluated.
            if !self.promotion.prior_alpha.is_finite() || self.promotion.prior_alpha <= 0.0 {
                return Err(ConfigError::invalid(
                    "promotion.prior_alpha",
                    "must be a finite value greater than zero",
                ));
            }
            if !self.promotion.prior_beta.is_finite() || self.promotion.prior_beta <= 0.0 {
                return Err(ConfigError::invalid(
                    "promotion.prior_beta",
                    "must be a finite value greater than zero",
                ));
            }
            validate_promotion_rule(
                "promotion.default_k",
                "promotion.default_threshold",
                self.promotion.default_k,
                self.promotion.default_threshold,
                self.promotion.prior_alpha,
                self.promotion.prior_beta,
            )?;
            if self.promotion.default_category.trim().is_empty() {
                return Err(ConfigError::missing("promotion.default_category"));
            }
            for (category, rule) in &self.promotion.categories {
                validate_promotion_rule(
                    &format!("promotion.categories.{category}.k"),
                    &format!("promotion.categories.{category}.threshold"),
                    rule.k,
                    rule.threshold,
                    self.promotion.prior_alpha,
                    self.promotion.prior_beta,
                )?;
            }
        }
        if self.reliability.enabled {
            if !self.reliability.prior_alpha.is_finite() || self.reliability.prior_alpha <= 0.0 {
                return Err(ConfigError::invalid(
                    "reliability.prior_alpha",
                    "must be a finite value greater than zero",
                ));
            }
            if !self.reliability.prior_beta.is_finite() || self.reliability.prior_beta <= 0.0 {
                return Err(ConfigError::invalid(
                    "reliability.prior_beta",
                    "must be a finite value greater than zero",
                ));
            }
            if self.reliability.default_category.trim().is_empty() {
                return Err(ConfigError::missing("reliability.default_category"));
            }
            // Each weight is a Beta pseudo-count, so it must be finite and non-negative or it
            // could push a posterior parameter negative and break the bounded `[0, 1]` score.
            for (key, weight) in [
                ("reliability.w_contradict", self.reliability.w_contradict),
                (
                    "reliability.w_attest_invalid",
                    self.reliability.w_attest_invalid,
                ),
                ("reliability.w_agree", self.reliability.w_agree),
            ] {
                if !weight.is_finite() || weight < 0.0 {
                    return Err(ConfigError::invalid(
                        key,
                        "must be a finite value greater than or equal to zero",
                    ));
                }
            }
            // The asymmetry guard: a producer's agreement gain must stay strictly below its
            // contradiction decay, so reliability can never be farmed back to neutral by
            // pairing a corroboration against a contradiction. Both weights are already known
            // finite from the loop above, so a plain `>=` is well-defined here. The attester
            // channel is loss-only (no gain), so it needs no analogous guard.
            if self.reliability.w_agree >= self.reliability.w_contradict {
                return Err(ConfigError::invalid(
                    "reliability.w_agree",
                    "must be strictly less than reliability.w_contradict (agreement gain must \
                     not outpace contradiction decay)",
                ));
            }
        }
        if self.decay.enabled {
            // A zero half-life would divide the elapsed time by zero downstream; the pure
            // function treats it as inert, so reject it here where the misconfiguration is
            // visible rather than shipping a decay that silently never decays.
            for (key, half_life) in [
                (
                    "decay.episodic_half_life_secs",
                    self.decay.episodic_half_life_secs,
                ),
                (
                    "decay.semantic_half_life_secs",
                    self.decay.semantic_half_life_secs,
                ),
            ] {
                if half_life == 0 {
                    return Err(ConfigError::invalid(
                        key,
                        "must be greater than zero when decay is enabled",
                    ));
                }
            }
        }
        self.forgetting.validate()?;
        self.core_block.validate()?;
        self.drift.validate()?;
        self.consolidation_guard.validate()?;
        self.auth.validate()?;
        self.server.validate()?;
        // Validate every DECLARED deployment — active or not — so a broken inactive profile is
        // caught at load rather than only when it is later selected. The inner key is prefixed
        // with `deployments.<name>.` so an operator can locate the offending profile; the name
        // is a config KEY (a declared deployment name), never a secret value.
        for (name, deployment) in &self.deployments {
            deployment
                .auth
                .validate()
                .map_err(|error| error.prefixed_key(&format!("deployments.{name}.")))?;
            deployment
                .server
                .validate()
                .map_err(|error| error.prefixed_key(&format!("deployments.{name}.")))?;
        }
        Ok(())
    }
}

/// Validate one `(k, threshold)` promotion gate: `k >= 2` (a quorum of one is not a
/// quorum and reopens single-attester laundering), `0.5 < threshold <= 1.0` (at or
/// below `0.5` the uninformative prior alone could clear it; above `1.0` is unreachable
/// for a bounded posterior, so it would lock the category shut), and the threshold
/// **reachable** at that `k` under the prior. The `!(… )` form also rejects a `NaN`
/// threshold, which fails every ordered comparison.
///
/// The reachability check is the cross-field guard. The reliability-weighted posterior maxes
/// out at `(prior_alpha + k) / (prior_alpha + prior_beta + k)` when all `k` attesters are
/// perfectly reliable; if that ceiling is below the threshold the two AND-ed gates are mutually
/// unsatisfiable (`k` attesters can never clear the bar), so the policy would silently promote
/// nothing and `k` would mislead. Callers must validate the priors are finite and positive
/// before this runs.
fn validate_promotion_rule(
    k_key: &str,
    threshold_key: &str,
    k: u64,
    threshold: f64,
    prior_alpha: f64,
    prior_beta: f64,
) -> Result<(), ConfigError> {
    if k < 2 {
        return Err(ConfigError::invalid(
            k_key.to_owned(),
            "must be at least 2 (a quorum of one is not a quorum)",
        ));
    }
    if !(threshold > 0.5 && threshold <= 1.0) {
        return Err(ConfigError::invalid(
            threshold_key.to_owned(),
            "must be in the range (0.5, 1.0]",
        ));
    }
    let max_posterior = (prior_alpha + k as f64) / (prior_alpha + prior_beta + k as f64);
    if threshold > max_posterior {
        return Err(ConfigError::invalid(
            threshold_key.to_owned(),
            format!(
                "is unreachable with {k_key} = {k} under the prior (the posterior tops out at \
                 {max_posterior:.3}); lower the threshold or raise the count"
            ),
        ));
    }
    Ok(())
}

/// Whether an inference endpoint's transport is allowed (§8.4): `https://` anywhere, or
/// plain `http://` only to a loopback host (`localhost`, `127.0.0.1`, or `[::1]`).
///
/// Exposed so the embedding client can enforce the same rule at construction, not only
/// at config validation.
#[must_use]
pub fn endpoint_transport_is_allowed(endpoint: &str) -> bool {
    let lower = endpoint.trim().to_ascii_lowercase();
    if lower.starts_with("https://") {
        return true;
    }
    if let Some(rest) = lower.strip_prefix("http://") {
        let authority = rest.split('/').next().unwrap_or_default();
        return matches!(
            host_of_authority(authority),
            "localhost" | "127.0.0.1" | "::1"
        );
    }
    false
}

/// The host of a URL `authority` (`host`, `host:port`, `[ipv6]`, or `[ipv6]:port`), with
/// any IPv6 brackets stripped. Splitting the whole authority on `:` would shred an IPv6
/// literal, so a bracketed host is taken up to its closing `]` first.
fn host_of_authority(authority: &str) -> &str {
    if let Some(after_bracket) = authority.strip_prefix('[') {
        return after_bracket.split(']').next().unwrap_or_default();
    }
    authority.split(':').next().unwrap_or_default()
}
