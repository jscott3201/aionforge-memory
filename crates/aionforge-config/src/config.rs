//! The configuration tree and its defaults, validation, and derived views.

use std::path::{Path, PathBuf};

use aionforge_store::{DEFAULT_EMBEDDING_DIMENSION, StoreConfig, default_data_dir};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// The largest sane per-request embedder timeout (ten minutes). A larger value is almost
/// certainly a units mistake (seconds typed as milliseconds), so it is rejected rather than
/// hanging a capture or recall on a wedged endpoint.
const MAX_EMBEDDER_TIMEOUT_MS: u64 = 600_000;

/// The largest sane per-request completer timeout (ten minutes), for the same reason as the
/// embedder ceiling. Chat completions can be slower than embeddings, but ten minutes is the
/// outer bound before a stuck endpoint should be treated as unavailable.
const MAX_COMPLETER_TIMEOUT_MS: u64 = 600_000;

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
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// On-disk state: where the WAL, snapshots, and logs live.
    pub persistence: PersistenceConfig,
    /// The embedding/inference endpoint and the model identity it serves.
    pub embedder: EmbedderConfig,
    /// The optional chat/completion provider (off by default), for LLM distillation and other
    /// opt-in chat use.
    pub completer: CompleterConfig,
    /// Retrieval defaults applied when a query does not override them.
    pub retrieval: RetrievalConfig,
    /// Security posture toggles.
    pub security: SecurityConfig,
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

/// Optional chat/completion provider configuration (08 §1, M3.T07).
///
/// Off by default: chat use (LLM distillation and the like) is opt-in, so an unset completer
/// leaves the deterministic canonical path untouched. A single provider and model are declared
/// — there is no cost-first auto-routing — so the responding model family stays verifiable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompleterConfig {
    /// Whether the chat/completion client is on.
    pub enabled: bool,
    /// The provider wire format: `openai_chat` (OpenAI and any OpenAI-compatible local/open
    /// server), `openai_responses` (OpenAI's Responses API, used statelessly), or `anthropic`
    /// (Claude Messages). The chat client validates the exact value.
    pub provider: String,
    /// The base URL. `https://` is required unless the host is localhost. Include the version
    /// segment the provider expects (e.g. `.../v1`); the resource path is appended.
    pub endpoint: String,
    /// The model id sent on each request and recorded as the declared identity.
    pub model: String,
    /// The **name** of the environment variable holding the API key, or none for a local
    /// unauthenticated endpoint. The key itself never lives in the config; see
    /// [`Config::resolve_completer_api_key`].
    pub api_key_env: Option<String>,
    /// Per-request timeout, in milliseconds.
    pub timeout_ms: u64,
    /// The output-token cap sent on each request — required by the Anthropic provider, an upper
    /// bound for the OpenAI providers. A per-request value overrides it.
    pub max_tokens: u32,
}

impl Default for CompleterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "openai_chat".to_owned(),
            endpoint: "http://127.0.0.1:1234/v1".to_owned(),
            model: String::new(),
            api_key_env: None,
            timeout_ms: 60_000,
            max_tokens: 4096,
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
    /// The clock-skew tolerance in milliseconds for signed writes (06 §3): a write whose
    /// timestamp deviates from the substrate clock by more than this is rejected (replay/storm
    /// mitigation). Only consulted when `signed_writes` is on; bounded to
    /// `MAX_CLOCK_SKEW_TOLERANCE_MS`.
    pub clock_skew_tolerance_ms: u64,
    /// Whether capture-side redaction of configured patterns is on.
    pub redaction: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            signed_writes: false,
            clock_skew_tolerance_ms: 60_000,
            redaction: true,
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

    /// Resolve the completer's API key by reading the environment variable named in
    /// `completer.api_key_env`, through the supplied lookup. Mirrors [`Config::resolve_api_key`]
    /// for the chat/completion provider.
    ///
    /// # Errors
    /// Returns [`ConfigError::SecretEnvMissing`] (naming the variable, never a value) when a
    /// variable is named but unset.
    pub fn resolve_completer_api_key<F>(
        &self,
        lookup: F,
    ) -> Result<Option<SecretString>, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        match &self.completer.api_key_env {
            None => Ok(None),
            Some(name) => match lookup(name) {
                Some(value) => Ok(Some(SecretString::from(value))),
                None => Err(ConfigError::SecretEnvMissing(name.clone())),
            },
        }
    }

    /// Resolve the completer's API key from the process environment.
    ///
    /// # Errors
    /// See [`Config::resolve_completer_api_key`].
    pub fn resolve_completer_api_key_from_env(&self) -> Result<Option<SecretString>, ConfigError> {
        self.resolve_completer_api_key(|name| std::env::var(name).ok())
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
        if self.completer.enabled {
            if self.completer.provider.trim().is_empty() {
                return Err(ConfigError::missing("completer.provider"));
            }
            if self.completer.endpoint.trim().is_empty() {
                return Err(ConfigError::missing("completer.endpoint"));
            }
            if self.completer.model.trim().is_empty() {
                return Err(ConfigError::missing("completer.model"));
            }
            if !endpoint_transport_is_allowed(&self.completer.endpoint) {
                return Err(ConfigError::invalid(
                    "completer.endpoint",
                    "must use https:// unless the host is localhost",
                ));
            }
            if self.completer.timeout_ms == 0 {
                return Err(ConfigError::invalid(
                    "completer.timeout_ms",
                    "must be greater than zero",
                ));
            }
            if self.completer.timeout_ms > MAX_COMPLETER_TIMEOUT_MS {
                return Err(ConfigError::invalid(
                    "completer.timeout_ms",
                    "must be at most 600000 (ten minutes)",
                ));
            }
            if self.completer.max_tokens == 0 {
                return Err(ConfigError::invalid(
                    "completer.max_tokens",
                    "must be greater than zero",
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
        if self.security.signed_writes {
            if self.security.clock_skew_tolerance_ms == 0 {
                // A zero window rejects every signed write (skew is always >= 0), so it is a
                // configuration error, not a silent lockout.
                return Err(ConfigError::invalid(
                    "security.clock_skew_tolerance_ms",
                    "must be greater than zero when signed writes are on",
                ));
            }
            if self.security.clock_skew_tolerance_ms > MAX_CLOCK_SKEW_TOLERANCE_MS {
                return Err(ConfigError::invalid(
                    "security.clock_skew_tolerance_ms",
                    "must be at most 300000 (five minutes)",
                ));
            }
        }
        Ok(())
    }
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
