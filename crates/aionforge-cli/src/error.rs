//! Error types for the CLI boundary.

#[derive(Debug, thiserror::Error)]
pub(crate) enum CliError {
    #[error(transparent)]
    Config(Box<aionforge_config::ConfigError>),

    #[error(transparent)]
    Embed(Box<aionforge_embed::EmbedError>),

    #[error(transparent)]
    Store(Box<aionforge_store::StoreError>),

    #[error(transparent)]
    Engine(Box<aionforge::EngineError>),

    #[error("could not serve MCP: {0}")]
    Serve(String),

    #[error("could not render doctor report: {0}")]
    Format(#[from] std::fmt::Error),

    #[error("could not serialize doctor report: {0}")]
    Json(#[from] serde_json::Error),

    #[error("could not write command output: {0}")]
    Io(#[from] std::io::Error),
}

impl From<aionforge_config::ConfigError> for CliError {
    fn from(error: aionforge_config::ConfigError) -> Self {
        Self::Config(Box::new(error))
    }
}

impl From<aionforge_embed::EmbedError> for CliError {
    fn from(error: aionforge_embed::EmbedError) -> Self {
        Self::Embed(Box::new(error))
    }
}

impl From<aionforge_store::StoreError> for CliError {
    fn from(error: aionforge_store::StoreError) -> Self {
        Self::Store(Box::new(error))
    }
}

impl From<aionforge::EngineError> for CliError {
    fn from(error: aionforge::EngineError) -> Self {
        Self::Engine(Box::new(error))
    }
}

impl From<aionforge_mcp::StreamableHttpConfigError> for CliError {
    fn from(error: aionforge_mcp::StreamableHttpConfigError) -> Self {
        Self::Serve(error.to_string())
    }
}
