//! The configuration tree and its defaults, validation, and derived views.

use std::path::{Path, PathBuf};

use aionforge_store::{DEFAULT_EMBEDDING_DIMENSION, StoreConfig, default_data_dir};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

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
    /// Whether capture-side redaction of configured patterns is on.
    pub redaction: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            signed_writes: false,
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
