//! OIDC discovery and RS256-pinned JWT validation for the resource server.
//!
//! [`JwtValidator`] turns an [`IssuerConfig`](aionforge_config::IssuerConfig) into a token
//! verifier: it resolves the issuer's JWKS endpoint (from the config or via OIDC discovery),
//! caches the signing keys by `kid` in memory, and validates bearer tokens with a
//! [`Validation`](jsonwebtoken::Validation) pinned to the issuer's RSA algorithms. A
//! successful [`JwtValidator::validate`] returns [`VerifiedClaims`] — the verified `sub`/`iss`/
//! `aud` plus the full raw claim map for the PR3 principal mapper.
//!
//! # Security posture
//! * **Algorithm pinning.** The validation allow-list is exactly the issuer's `allowed_algs`
//!   (RSA-only). `alg=none` and `HS*` algorithm confusion are rejected at the library boundary.
//! * **Exact issuer/audience matching.** The issuer is compared byte-for-byte (the Auth0
//!   trailing slash and the Entra no-slash forms are distinct and never normalized); the
//!   audience is matched exactly.
//! * **`kid` rotation, bounded and rate-limited.** An unknown `kid` triggers at most one JWKS
//!   refetch, and refetches are rate-limited by a cooldown across calls, so a flood of tokens
//!   bearing distinct `kid`s cannot drive a fetch storm at the issuer's JWKS endpoint. A miss
//!   inside the cooldown fails closed with [`AuthError::NoMatchingKey`] from the cache.
//! * **SSRF-guarded fetches.** Every `jwks_uri` — from the config override or a discovery
//!   document — must share the configured issuer's origin (scheme + host + port), and the HTTP
//!   client follows no redirects, so neither fetch can be pivoted at cloud-metadata or internal
//!   services. Discovery and JWKS bodies are size-capped to defeat a memory-exhaustion DoS.
//! * **Public-key verify only.** The validator never holds a private key.
//! * **Fail closed, leak nothing.** Network, parse, and crypto failures become an
//!   [`AuthError`]; no method panics, and no error message contains the token, a claim value,
//!   or key material.
//!
//! # Library boundary (fork#3)
//! No `jsonwebtoken` type appears in this crate's public API. `DecodingKey`, `Validation`, and
//! the library's claim and error types are confined to the internal modules so the JWT library
//! cannot leak into the PR3 mapper or any consuming crate.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod discovery;
mod error;
mod fetch;
mod jwks;
mod validate;
mod validator;

pub use error::AuthError;
pub use validate::VerifiedClaims;
pub use validator::JwtValidator;
