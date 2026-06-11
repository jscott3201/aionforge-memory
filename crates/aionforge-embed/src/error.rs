//! The embedding-client error space.

use aionforge_domain::DomainError;

/// An error from the embedding client.
///
/// [`Unavailable`](EmbedError::Unavailable) means the endpoint could not be reached or
/// returned a server error. Read paths may degrade to lexical and graph signals; write
/// paths that require vectors can fail closed. Every other variant is a hard error the
/// caller should surface.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[non_exhaustive]
pub enum EmbedError {
    /// The endpoint could not be reached, timed out, or returned a 5xx.
    #[error("embedding endpoint unavailable: {0}")]
    Unavailable(String),

    /// The endpoint returned a non-success status that is not a 5xx (e.g. a 4xx).
    #[error("embedding endpoint returned HTTP status {status}")]
    Status {
        /// The HTTP status code.
        status: u16,
    },

    /// The response body was missing or not the expected shape.
    #[error("could not decode the embedding response: {0}")]
    Decode(String),

    /// The endpoint returned a different number of vectors than inputs sent (§8.1).
    #[error("expected {expected} embeddings but the endpoint returned {actual}")]
    WrongCount {
        /// How many inputs were sent.
        expected: usize,
        /// How many vectors came back.
        actual: usize,
    },

    /// A returned vector's dimension disagreed with the model's declared dimension.
    #[error("embedding dimension {actual} does not match the model dimension {expected}")]
    DimensionMismatch {
        /// The model's declared dimension.
        expected: u32,
        /// The dimension actually returned.
        actual: usize,
    },

    /// A returned vector was not a valid embedding (empty or non-finite).
    #[error("invalid embedding from the endpoint: {0}")]
    Invalid(#[from] DomainError),

    /// The client could not be constructed (a bad endpoint URL, for instance).
    #[error("invalid embedding client configuration: {0}")]
    Config(String),
}

impl EmbedError {
    /// Whether this error means the endpoint is unavailable, so a caller with a
    /// non-vector fallback may degrade rather than fail the operation (§8.1).
    #[must_use]
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}
