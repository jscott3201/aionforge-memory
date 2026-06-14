//! OIDC discovery: resolving an issuer's JWKS endpoint.
//!
//! When the [`IssuerConfig`](aionforge_config::IssuerConfig) does not pin a `jwks_uri`, the
//! validator fetches the issuer's `/.well-known/openid-configuration` document (OpenID
//! Connect Discovery 1.0) and reads `jwks_uri` from it. The fetch happens once at validator
//! construction (a stateless, no-background-task v1): there is no long-lived discovery cache
//! to go stale, so a key-store move is picked up on the next validator build. The document's
//! `issuer` field is checked **byte-for-byte** against the configured issuer, and the returned
//! `jwks_uri` must share that issuer's origin (scheme + host + port) — closing the
//! discovery-spoofing seam where a document points the validator at attacker or internal keys.
//! The body is size-capped (see [`crate::fetch`]) and the client follows no redirects.

use serde::Deserialize;

use crate::error::AuthError;
use crate::fetch::{fetch_body_capped, jwks_uri_origin_is_allowed};

/// The subset of the OIDC discovery document this crate reads.
///
/// Only `issuer` (verified against the configured value) and `jwks_uri` (the signing-key
/// endpoint) matter for RS256 validation; every other discovery field is ignored.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DiscoveryDocument {
    /// The issuer identifier the document asserts. Must equal the configured issuer
    /// byte-for-byte, or the document is rejected.
    pub(crate) issuer: String,
    /// The JWKS endpoint carrying this issuer's signing keys.
    pub(crate) jwks_uri: String,
}

/// Build the discovery URL for an issuer per OpenID Connect Discovery 1.0.
///
/// The well-known suffix is appended to the issuer with exactly one `/` separator,
/// tolerating an issuer that already ends in `/` (Auth0) or does not (Entra v2). This does
/// not mutate the issuer used for claim matching — only the URL fetched here.
fn discovery_url(issuer: &str) -> String {
    let trimmed = issuer.trim_end_matches('/');
    format!("{trimmed}/.well-known/openid-configuration")
}

/// Fetch and parse the issuer's discovery document, returning its `jwks_uri`.
///
/// The document's asserted `issuer` is compared byte-for-byte against `issuer`; a mismatch is
/// a [`AuthError::Discovery`]. The returned `jwks_uri` must additionally share the issuer's
/// origin (scheme + host + port), so a document that asserts the right issuer but points
/// `jwks_uri` at an internal or attacker host is rejected rather than fetched.
///
/// # Errors
/// Returns [`AuthError::Discovery`] if the endpoint is unreachable, returns a non-success
/// status, exceeds the body cap, is not valid JSON, asserts a different issuer, or returns a
/// `jwks_uri` whose origin differs from the issuer's.
pub(crate) async fn resolve_jwks_uri(
    issuer: &str,
    client: &reqwest::Client,
) -> Result<String, AuthError> {
    let url = discovery_url(issuer);
    let response = client.get(&url).send().await.map_err(|error| {
        AuthError::Discovery(format!("could not reach the discovery endpoint: {error}"))
    })?;
    if !response.status().is_success() {
        return Err(AuthError::Discovery(format!(
            "discovery endpoint returned HTTP status {}",
            response.status().as_u16()
        )));
    }
    let body = fetch_body_capped(response, AuthError::Discovery).await?;
    let document: DiscoveryDocument = serde_json::from_str(&body).map_err(|error| {
        AuthError::Discovery(format!("could not parse the discovery document: {error}"))
    })?;
    if document.issuer != issuer {
        return Err(AuthError::Discovery(
            "the discovery document asserts a different issuer than configured".to_owned(),
        ));
    }
    if !jwks_uri_origin_is_allowed(&document.jwks_uri, issuer) {
        return Err(AuthError::Discovery(
            "the discovery document's jwks_uri is not on the issuer's origin".to_owned(),
        ));
    }
    Ok(document.jwks_uri)
}

#[cfg(test)]
mod tests {
    use super::discovery_url;

    #[test]
    fn appends_one_slash_for_an_entra_no_slash_issuer() {
        assert_eq!(
            discovery_url("https://login.microsoftonline.com/tid/v2.0"),
            "https://login.microsoftonline.com/tid/v2.0/.well-known/openid-configuration"
        );
    }

    #[test]
    fn tolerates_an_auth0_trailing_slash_issuer() {
        assert_eq!(
            discovery_url("https://tenant.us.auth0.com/"),
            "https://tenant.us.auth0.com/.well-known/openid-configuration"
        );
    }
}
