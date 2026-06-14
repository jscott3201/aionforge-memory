//! Shared HTTP-fetch hardening for the discovery and JWKS endpoints.
//!
//! Two SSRF/DoS controls live here so the discovery fetch and the JWKS fetch enforce them
//! identically:
//!
//! * **Origin pinning ([`jwks_uri_origin_is_allowed`]).** A `jwks_uri` — whether it came from
//!   the config override or from a discovery document — must be an `https` (or `http` loopback)
//!   URL whose origin (scheme + host + port) equals the configured issuer's origin. This stops
//!   a malicious or misconfigured issuer/discovery document from pointing key fetches at
//!   cloud-metadata (`169.254.169.254`) or internal services. The host comparison mirrors the
//!   config layer's `issuer_origin_is_allowed`: equality only (no prefix match), userinfo is a
//!   spoof and rejected, IPv6 brackets are kept whole.
//! * **Bounded body ([`fetch_body_capped`]).** The response body is streamed in chunks into a
//!   buffer with a hard ceiling, so a hostile endpoint cannot stream a multi-gigabyte body
//!   within the request timeout and exhaust memory. The client itself is built with
//!   `redirect(Policy::none())` (see [`crate::validator`]) so neither fetch ever follows a
//!   redirect to an unvalidated target.

/// The largest discovery/JWKS body accepted, in bytes (1 MiB). Real discovery documents and
/// JWKS sets are a few kilobytes; anything past this ceiling is treated as hostile and the
/// fetch fails closed rather than buffering an unbounded body into memory.
pub(crate) const MAX_BODY_BYTES: usize = 1024 * 1024;

/// The origin (scheme + host + optional port) of a URL, for same-origin comparison.
#[derive(PartialEq, Eq)]
struct Origin {
    /// `https` or `http`; any other scheme yields `None` from [`Origin::parse`].
    scheme: &'static str,
    /// The bare host with userinfo stripped, IPv6 brackets kept whole, port retained.
    authority: String,
}

impl Origin {
    /// Parse the origin of `url`, accepting only `https` and `http`. Userinfo (`user@host`) is
    /// rejected (a spoof, never part of an origin); the authority keeps its `host[:port]` so a
    /// differing port is a differing origin. Returns `None` for any non-http(s) scheme, an
    /// empty host, or a userinfo-bearing authority.
    fn parse(url: &str) -> Option<Self> {
        let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
            ("https", rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            ("http", rest)
        } else {
            return None;
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        if authority.contains('@') {
            return None;
        }
        // The host (port stripped) must be non-empty; reject a structurally-empty authority.
        let host = if let Some(after_bracket) = authority.strip_prefix('[') {
            match after_bracket.split_once(']') {
                Some((inner, _)) => inner,
                None => return None,
            }
        } else {
            authority.split(':').next().unwrap_or_default()
        };
        if host.is_empty() {
            return None;
        }
        Some(Self {
            scheme,
            authority: authority.to_owned(),
        })
    }
}

/// Whether a candidate `jwks_uri` shares the configured issuer's origin.
///
/// Both must parse as an `https`/`http` URL with no userinfo and a non-empty host, and their
/// scheme + host + port must be equal. The issuer is the value the config layer already vetted
/// (`https` or an `http` loopback), so requiring origin equality transitively confines the
/// `jwks_uri` to that same vetted, non-internal origin.
pub(crate) fn jwks_uri_origin_is_allowed(jwks_uri: &str, issuer: &str) -> bool {
    match (Origin::parse(jwks_uri), Origin::parse(issuer)) {
        (Some(uri), Some(iss)) => uri == iss,
        _ => false,
    }
}

/// Fetch a response body into a `String`, aborting if it exceeds [`MAX_BODY_BYTES`].
///
/// A `Content-Length` past the ceiling is rejected before any body is read; because that header
/// may be absent or untruthful, the body is then streamed chunk-by-chunk and the running total
/// is re-checked against the ceiling so a chunked/lying endpoint is still capped. `on_error`
/// maps each failure mode to the caller's domain error (discovery vs JWKS refresh).
pub(crate) async fn fetch_body_capped<E>(
    mut response: reqwest::Response,
    on_error: impl Fn(String) -> E,
) -> Result<String, E> {
    if let Some(len) = response.content_length()
        && len > MAX_BODY_BYTES as u64
    {
        return Err(on_error(
            "the endpoint body exceeds the maximum allowed size".to_owned(),
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| on_error(format!("could not read the endpoint body: {error}")))?
    {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(on_error(
                "the endpoint body exceeds the maximum allowed size".to_owned(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body)
        .map_err(|_| on_error("the endpoint body was not valid UTF-8".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::{MAX_BODY_BYTES, jwks_uri_origin_is_allowed};

    #[test]
    fn same_origin_jwks_uri_is_allowed() {
        assert!(jwks_uri_origin_is_allowed(
            "https://issuer.example/keys/jwks.json",
            "https://issuer.example/"
        ));
        // A port that matches is still the same origin.
        assert!(jwks_uri_origin_is_allowed(
            "http://127.0.0.1:8080/jwks",
            "http://127.0.0.1:8080/"
        ));
    }

    #[test]
    fn cross_origin_and_metadata_targets_are_refused() {
        for (uri, issuer) in [
            // Cloud metadata while the issuer stays legit (the SSRF pivot the guard closes).
            (
                "http://169.254.169.254/latest/meta-data/",
                "https://issuer.example/",
            ),
            // Loopback exfil target while the issuer is a real host.
            ("http://127.0.0.1:3918/mcp", "https://issuer.example/"),
            // A different host entirely.
            ("https://attacker.example/jwks", "https://issuer.example/"),
            // Same host, different port is a different origin.
            (
                "https://issuer.example:8443/jwks",
                "https://issuer.example/",
            ),
            // Scheme downgrade is a different origin.
            ("http://issuer.example/jwks", "https://issuer.example/"),
            // Userinfo spoof: the real host is `attacker.example`.
            (
                "https://issuer.example@attacker.example/jwks",
                "https://issuer.example/",
            ),
            // A non-http(s) scheme is never allowed.
            ("file:///etc/passwd", "https://issuer.example/"),
        ] {
            assert!(
                !jwks_uri_origin_is_allowed(uri, issuer),
                "{uri} must not be allowed for issuer {issuer}"
            );
        }
    }

    #[test]
    fn the_body_ceiling_is_one_mib() {
        assert_eq!(MAX_BODY_BYTES, 1024 * 1024);
    }
}
