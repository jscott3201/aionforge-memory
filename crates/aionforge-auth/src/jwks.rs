//! JWKS fetch, parse, and the in-memory `kid -> key` cache.
//!
//! The cache holds one [`jsonwebtoken::DecodingKey`] per `kid`, built from the RSA `n`/`e`
//! components of each JWK (RFC 7517). It is populated at validator construction and refreshed
//! at most **once per `validate` call** on an unknown-`kid` miss — there is no TTL timer and
//! no retry loop, so a key-rotation race cannot drive an unbounded fetch storm. A JWK without
//! a `kid`, or one that is not an RSA key usable for RS256/384/512, is skipped rather than
//! failing the whole document, so an issuer that publishes a mixed key set (e.g. an EC key
//! alongside its RSA signing keys) still yields a usable cache.
//!
//! `DecodingKey` is jsonwebtoken's type; it never escapes this crate (fork#3). The cache is
//! kept behind a `Mutex` for concurrent `validate` calls — see [`crate::validator`].

use std::collections::BTreeMap;

use jsonwebtoken::DecodingKey;
use serde::Deserialize;

use crate::error::AuthError;
use crate::fetch::fetch_body_capped;

/// The raw JWKS document: a `keys` array of JWK entries (RFC 7517 §5).
#[derive(Debug, Deserialize)]
struct JwksDocument {
    keys: Vec<Jwk>,
}

/// One JWK entry. Only the fields needed to build an RSA verify key are read; unknown fields
/// (`use`, `alg`, `x5c`, …) are ignored, and a non-RSA entry is skipped at build time.
#[derive(Debug, Deserialize)]
struct Jwk {
    /// The key type. Only `RSA` yields a usable RS256/384/512 verify key here.
    kty: String,
    /// The key id used to select this key for a token. An entry without it cannot be
    /// addressed by a token `kid` and is skipped.
    kid: Option<String>,
    /// The RSA modulus, base64url (no padding) per RFC 7518 §6.3.1.1.
    n: Option<String>,
    /// The RSA exponent, base64url (no padding) per RFC 7518 §6.3.1.2.
    e: Option<String>,
}

/// An in-memory map from `kid` to its decoding key, built from a JWKS document.
///
/// One cache belongs to one validator (one issuer). Lookups are by `kid`; a populated cache
/// answers from memory with no network call.
#[derive(Default)]
pub(crate) struct JwksCache {
    keys: BTreeMap<String, DecodingKey>,
}

impl JwksCache {
    /// Look up the decoding key for a `kid`, if present.
    pub(crate) fn get(&self, kid: &str) -> Option<&DecodingKey> {
        self.keys.get(kid)
    }
}

/// Parse a JWKS document body into a `kid -> DecodingKey` cache.
///
/// Entries that are not RSA, lack a `kid`, lack `n`/`e`, or whose components do not form a
/// valid RSA key are skipped (not fatal), so a mixed key set still produces a usable cache.
///
/// # Errors
/// Returns [`AuthError::JwksRefresh`] only if the body itself is not a valid JWKS JSON
/// document; a per-entry problem is skipped, never surfaced as an error.
fn parse_jwks(body: &str) -> Result<JwksCache, AuthError> {
    let document: JwksDocument = serde_json::from_str(body).map_err(|error| {
        AuthError::JwksRefresh(format!("could not parse the JWKS document: {error}"))
    })?;
    let mut keys = BTreeMap::new();
    for jwk in document.keys {
        if jwk.kty != "RSA" {
            continue;
        }
        let (Some(kid), Some(n), Some(e)) = (jwk.kid, jwk.n, jwk.e) else {
            continue;
        };
        // `from_rsa_components` takes the base64url-encoded `n`/`e` strings directly (it
        // decodes them internally); an entry whose components are malformed is skipped.
        if let Ok(key) = DecodingKey::from_rsa_components(&n, &e) {
            keys.insert(kid, key);
        }
    }
    Ok(JwksCache { keys })
}

/// Fetch the JWKS document at `jwks_uri` and parse it into a fresh cache.
///
/// The body is read with a hard size cap (see [`crate::fetch`]) so a hostile or compromised
/// endpoint cannot stream an unbounded body and exhaust memory.
///
/// # Errors
/// Returns [`AuthError::JwksRefresh`] if the endpoint is unreachable, returns a non-success
/// status, exceeds the body cap, or the body is not a valid JWKS document.
pub(crate) async fn fetch_jwks(
    jwks_uri: &str,
    client: &reqwest::Client,
) -> Result<JwksCache, AuthError> {
    let response = client.get(jwks_uri).send().await.map_err(|error| {
        AuthError::JwksRefresh(format!("could not reach the JWKS endpoint: {error}"))
    })?;
    if !response.status().is_success() {
        return Err(AuthError::JwksRefresh(format!(
            "JWKS endpoint returned HTTP status {}",
            response.status().as_u16()
        )));
    }
    let body = fetch_body_capped(response, AuthError::JwksRefresh).await?;
    parse_jwks(&body)
}

#[cfg(test)]
mod tests {
    use super::parse_jwks;

    #[test]
    fn skips_a_key_without_a_kid() {
        let body = r#"{"keys":[{"kty":"RSA","n":"AQAB","e":"AQAB"}]}"#;
        let cache = parse_jwks(body).expect("valid jwks json");
        assert!(cache.keys.is_empty(), "a kid-less key is not addressable");
    }

    #[test]
    fn skips_a_non_rsa_key() {
        let body = r#"{"keys":[{"kty":"EC","kid":"ec-1","crv":"P-256","x":"AQAB","y":"AQAB"}]}"#;
        let cache = parse_jwks(body).expect("valid jwks json");
        assert!(
            cache.keys.is_empty(),
            "an EC key is not an RS256 verify key"
        );
    }

    #[test]
    fn rejects_a_non_jwks_body() {
        assert!(parse_jwks("not json").is_err());
    }
}
