//! Layered configuration for Aionforge Memory.
//!
//! One [`Config`] is assembled from four layers, lowest precedence first:
//!
//! 1. **Defaults** — the compiled-in [`Config::default`], so an empty environment still
//!    yields a working config.
//! 2. **File** — a TOML file (by default `~/.aionforge/config.toml`); a missing file is
//!    simply skipped.
//! 3. **Environment** — variables prefixed `AIONFORGE_`, with `__` separating the
//!    section from the field. `AIONFORGE_EMBEDDER__DIMENSION=1024` sets
//!    `embedder.dimension`; `AIONFORGE_PERSISTENCE__DATA_DIR=/srv/mem` sets the data
//!    directory.
//! 4. **Flags** — a provider the caller merges last (command-line flags map here), so
//!    they override everything below.
//!
//! A later layer wins over an earlier one, key by key. After assembly the config is
//! validated: a missing required key or an out-of-range value fails with a message that
//! names the key and never quotes a value.
//!
//! Secrets stay out of the config entirely. The embedder records the **name** of the
//! environment variable holding its API key ([`EmbedderConfig::api_key_env`]); the key
//! is read on demand by [`Config::resolve_api_key`] into a [`secrecy::SecretString`]
//! that redacts in logs and zeroizes on drop. Logging a [`Config`] can never leak a key.
//!
//! # Example
//!
//! A `config.toml` (every key is optional):
//!
//! ```toml
//! [persistence]
//! data_dir = "/srv/aionforge"
//!
//! [embedder]
//! endpoint = "https://api.example.com/v1"
//! model = "codestral-embed-2505"
//! dimension = 1536
//! api_key_env = "AIONFORGE_API_KEY"
//!
//! [retrieval]
//! default_k = 12
//! fusion_constant = 60
//!
//! [security]
//! signed_writes = true
//!
//! [core_block]
//! redline_requires_human = true
//! human_attester_ids = ["0197b0aa-3c5e-8000-8000-000000000000"]
//! ```

mod auth;
mod config;
mod core_block;
mod drift;
mod error;
mod forgetting;
mod guard;
mod load;

pub use auth::{AuthConfig, IssuerConfig};
pub use config::{
    CategoryPromotionRule, CompleterConfig, Config, DecayConfig, EmbedderConfig, PersistenceConfig,
    PromotionConfig, ReliabilityConfig, RetrievalConfig, SecurityConfig,
    endpoint_transport_is_allowed,
};
pub use core_block::{CoreBlockConfig, CoreEditRuleConfig};
pub use drift::DriftConfig;
pub use error::ConfigError;
pub use forgetting::ForgettingConfig;
pub use guard::{ConsolidationGuardConfig, GuardMode};
pub use load::default_config_path;
