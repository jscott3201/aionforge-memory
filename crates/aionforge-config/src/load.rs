//! Layered loading: defaults, then a TOML file, then the environment, then flags.

use std::path::{Path, PathBuf};

use aionforge_store::default_data_dir;
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};

use crate::config::Config;
use crate::error::ConfigError;

/// The default config file path: `<data_dir>/config.toml` (so `~/.aionforge/config.toml`).
#[must_use]
pub fn default_config_path() -> PathBuf {
    default_data_dir().join("config.toml")
}

impl Config {
    /// Build the layered figment, lowest precedence first:
    ///
    /// 1. the compiled-in defaults,
    /// 2. the TOML file at `config_path` (ignored if it does not exist),
    /// 3. environment variables prefixed `AIONFORGE_`, nested on `__`
    ///    (`AIONFORGE_EMBEDDER__DIMENSION` sets `embedder.dimension`).
    ///
    /// Callers add the highest-precedence flags layer by merging their own provider onto
    /// the returned figment before calling [`Config::from_figment`].
    #[must_use]
    pub fn figment(config_path: &Path) -> Figment {
        Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::file(config_path))
            .merge(Env::prefixed("AIONFORGE_").split("__"))
    }

    /// Load from the default file path plus the environment, then validate.
    ///
    /// # Errors
    /// Returns [`ConfigError::Load`] if a layer cannot be read or parsed, or a
    /// validation error if a required key is missing or a value is out of range.
    pub fn load() -> Result<Config, ConfigError> {
        Self::from_figment(Self::figment(&default_config_path()))
    }

    /// Extract and validate a [`Config`] from a prepared figment.
    ///
    /// The flags layer, when present, is a provider the caller has already merged onto
    /// the figment (it wins because it was merged last).
    ///
    /// # Errors
    /// Returns [`ConfigError::Load`] if extraction fails, or a validation error from
    /// [`Config::validate`].
    pub fn from_figment(figment: Figment) -> Result<Config, ConfigError> {
        let config: Config = figment
            .extract()
            .map_err(|error| ConfigError::Load(error.to_string()))?;
        config.validate()?;
        Ok(config)
    }
}
