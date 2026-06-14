//! The authentication error space.
//!
//! Every variant carries a short, non-secret message. A bearer token is a secret, so no
//! variant ever embeds the token, a claim value, a signature, or a key — only the kind of
//! failure and, at most, a non-secret identifier such as a `kid` or an endpoint URL. The
//! validator fails closed: a network, parse, or crypto failure is mapped to one of these
//! variants and returned, never a panic.

/// An error from OIDC discovery, JWKS handling, or JWT validation.
///
/// The variants separate transport/availability failures
/// ([`Discovery`](AuthError::Discovery), [`JwksRefresh`](AuthError::JwksRefresh)) from the
/// security verdicts a caller may want to distinguish ([`IssuerMismatch`](AuthError::IssuerMismatch),
/// [`AudienceMismatch`](AuthError::AudienceMismatch),
/// [`AlgorithmNotAllowed`](AuthError::AlgorithmNotAllowed),
/// [`NoMatchingKey`](AuthError::NoMatchingKey),
/// [`InvalidToken`](AuthError::InvalidToken)). No message contains the token, a claim value,
/// or any key material.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[non_exhaustive]
pub enum AuthError {
    /// The issuer's `/.well-known/openid-configuration` document could not be fetched or
    /// parsed. The message names the failure mode, never a secret.
    #[error("OIDC discovery failed: {0}")]
    Discovery(String),

    /// The JWKS document could not be fetched or parsed. The message names the failure
    /// mode, never a secret.
    #[error("JWKS refresh failed: {0}")]
    JwksRefresh(String),

    /// The token's `kid` was not present in the JWKS, even after a single bounded refetch.
    /// The message carries only the non-secret `kid` identifier.
    #[error("no signing key in the JWKS matches the token kid: {0}")]
    NoMatchingKey(String),

    /// The token was malformed, its signature did not verify, or a time-based claim
    /// (`exp`/`nbf`) failed outside the configured leeway. The message names the
    /// failure mode, never the token or a claim value.
    #[error("invalid token: {0}")]
    InvalidToken(String),

    /// The token's `iss` claim did not match the configured issuer **byte-for-byte** (the
    /// Auth0 trailing slash and the Entra no-slash forms are distinct and not normalized).
    /// The message states the verdict without quoting either issuer value.
    #[error("token issuer does not match the configured issuer (exact match required)")]
    IssuerMismatch,

    /// The token's `aud` claim did not match the configured audience. The message states
    /// the verdict without quoting the audience value.
    #[error("token audience does not match the configured audience")]
    AudienceMismatch,

    /// The token's `alg` header was not one of the configured RSA algorithms (`RS256`,
    /// `RS384`, `RS512`). This rejects `alg=none` and any symmetric `HS*` algorithm-confusion
    /// attempt. The message carries only the non-secret algorithm name.
    #[error("token algorithm is not in the allowed set: {0}")]
    AlgorithmNotAllowed(String),
}
