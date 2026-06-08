//! The typed error space for constructing and validating domain values.

use thiserror::Error;

/// Errors produced when constructing or validating a domain value.
///
/// These are pure validation failures (malformed identifier, out-of-range score,
/// empty embedding). I/O and storage errors live in the layers that perform I/O.
#[derive(Debug, Error, Clone, PartialEq)]
#[non_exhaustive]
pub enum DomainError {
    /// An identifier was empty or not a valid UUID string.
    #[error("invalid identifier: `{0}`")]
    InvalidId(String),

    /// A namespace string did not match a known namespace form.
    #[error("invalid namespace: `{0}`")]
    InvalidNamespace(String),

    /// An embedding was empty or contained a non-finite component.
    #[error("invalid embedding: {0}")]
    InvalidEmbedding(String),

    /// A content hash string was not a valid blake3 hex digest.
    #[error("invalid content hash: `{0}`")]
    InvalidContentHash(String),

    /// A score expected in the closed range `[0, 1]` was out of range.
    #[error("value {1} for `{0}` is outside the range [0, 1]")]
    OutOfUnitRange(&'static str, f64),
}
