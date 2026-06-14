//! RS256-pinned token decode and claim extraction.
//!
//! The [`jsonwebtoken::Validation`] is built **once per validator** with an explicit
//! algorithm allow-list restricted to the issuer's `allowed_algs` (already constrained by
//! the config layer to `RS256`/`RS384`/`RS512`). Because the allow-list is never empty and
//! the token's header `alg` must be a member, two attacks are closed at the library boundary:
//!
//! * `alg=none` — `none` is not a `jsonwebtoken::Algorithm` variant, so the header fails to
//!   parse into one of the allowed algorithms and the token is rejected before any key use.
//! * `HS*` confusion (signing an `HS256` token with the RSA public key as the HMAC secret) —
//!   `HS256` is not in the RSA allow-list, so the header `alg` check rejects it; the RSA
//!   `DecodingKey` is never offered to an HMAC verifier.
//!
//! `set_issuer` uses the configured issuer string verbatim (no trailing-slash normalization),
//! `set_audience` the configured audience, and `leeway` the configured clock-skew window
//! applied to `exp`/`nbf`. None of jsonwebtoken's types appear in the returned
//! [`VerifiedClaims`] (fork#3).

use std::collections::BTreeMap;

use jsonwebtoken::{Algorithm, DecodingKey, TokenData, Validation};

use crate::error::AuthError;

/// The verified claims of a successfully validated token.
///
/// `sub`/`iss`/`aud` are surfaced as their own fields for the common case; `claims` carries
/// the full raw claim map so the PR3 principal mapper can read issuer-specific claims (teams,
/// operator permission, a stable agent-id claim) without this crate knowing their names. The
/// values are present **only** because the signature, issuer, audience, and time bounds all
/// passed — an unverified token never yields a `VerifiedClaims`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedClaims {
    /// The token subject (`sub`).
    pub sub: String,
    /// The issuer (`iss`), matched byte-for-byte against the configured issuer.
    pub iss: String,
    /// The audience (`aud`) this validator was configured for and that the token satisfied.
    pub aud: String,
    /// The full raw claim map, for downstream issuer-specific mapping (PR3).
    pub claims: BTreeMap<String, serde_json::Value>,
}

/// Map a configured algorithm name to the jsonwebtoken [`Algorithm`]. Only the RSA family is
/// accepted; anything else (including a stray `HS*`/`none` that slipped a malformed config)
/// is reported as not allowed rather than silently widening the allow-list.
fn rsa_algorithm(name: &str) -> Result<Algorithm, AuthError> {
    match name {
        "RS256" => Ok(Algorithm::RS256),
        "RS384" => Ok(Algorithm::RS384),
        "RS512" => Ok(Algorithm::RS512),
        other => Err(AuthError::AlgorithmNotAllowed(other.to_owned())),
    }
}

/// Build the RS256-pinned [`Validation`] for an issuer.
///
/// The algorithm list is set to exactly the (RSA-only) `allowed_algs`, never left empty and
/// never widened. Issuer and audience are exact-match; `exp`/`nbf` are validated within
/// `leeway_secs` (`jsonwebtoken` v9 does not validate `iat`).
///
/// # Errors
/// Returns [`AuthError::AlgorithmNotAllowed`] if `allowed_algs` is empty or names a non-RSA
/// algorithm (the config layer already forbids this; the check is defence in depth).
pub(crate) fn build_validation(
    issuer: &str,
    audience: &str,
    allowed_algs: &[String],
    leeway_secs: u64,
) -> Result<Validation, AuthError> {
    let mut algorithms = Vec::with_capacity(allowed_algs.len());
    for name in allowed_algs {
        algorithms.push(rsa_algorithm(name)?);
    }
    if algorithms.is_empty() {
        return Err(AuthError::AlgorithmNotAllowed(
            "the issuer's allowed algorithm set is empty".to_owned(),
        ));
    }
    // Seed with the first allowed algorithm, then set the full explicit list. `Validation::new`
    // already enables exp validation and audience validation; we add issuer + nbf + leeway and
    // require the spec claims we depend on.
    let mut validation = Validation::new(algorithms[0]);
    validation.algorithms = algorithms;
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[audience]);
    validation.validate_exp = true;
    validation.validate_nbf = true;
    validation.validate_aud = true;
    validation.leeway = leeway_secs;
    validation.set_required_spec_claims(&["exp", "iss", "aud"]);
    Ok(validation)
}

/// Decode and validate a token against a prepared key and validation, returning domain claims.
///
/// `configured_audience` is threaded through so [`VerifiedClaims::aud`] reports the audience
/// the token was proven to satisfy (the token's own `aud` may be an array; the validator has
/// already confirmed the configured value is a member).
///
/// # Errors
/// Maps each jsonwebtoken failure to the matching [`AuthError`] verdict; no token, claim
/// value, or key material appears in any message.
pub(crate) fn decode_and_validate(
    token: &str,
    key: &DecodingKey,
    validation: &Validation,
    configured_audience: &str,
) -> Result<VerifiedClaims, AuthError> {
    // A single decode verifies the RS256 signature, issuer, audience, and time bounds AND
    // yields the full claim map. Decoding straight into the map avoids a second, redundant
    // signature verification that a separate typed decode would cost on every valid token.
    let data: TokenData<BTreeMap<String, serde_json::Value>> =
        jsonwebtoken::decode(token, key, validation).map_err(map_jwt_error)?;
    let claims = data.claims;
    // `sub` is not in jsonwebtoken's required-spec-claims set, so assert its presence here.
    // `iss` is required and already matched the configured issuer byte-for-byte; we read the
    // token's own (matched) value for fidelity. Both must be JSON strings.
    let sub = claims
        .get("sub")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| AuthError::InvalidToken("token is missing the sub claim".to_owned()))?
        .to_owned();
    let iss = claims
        .get("iss")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| AuthError::InvalidToken("token is missing the iss claim".to_owned()))?
        .to_owned();
    Ok(VerifiedClaims {
        sub,
        iss,
        aud: configured_audience.to_owned(),
        claims,
    })
}

/// Translate a [`jsonwebtoken::errors::Error`] into the matching [`AuthError`] verdict.
///
/// The mapping keeps the security-relevant distinctions a caller may branch on (issuer,
/// audience, algorithm) separate from the catch-all malformed/signature/time failures, and
/// never embeds the token or a claim value.
fn map_jwt_error(error: jsonwebtoken::errors::Error) -> AuthError {
    use jsonwebtoken::errors::ErrorKind;
    match error.kind() {
        ErrorKind::InvalidIssuer => AuthError::IssuerMismatch,
        ErrorKind::InvalidAudience => AuthError::AudienceMismatch,
        ErrorKind::InvalidAlgorithm | ErrorKind::MissingAlgorithm => {
            AuthError::AlgorithmNotAllowed(
                "token algorithm is not an allowed RSA algorithm".to_owned(),
            )
        }
        ErrorKind::ExpiredSignature => {
            AuthError::InvalidToken("token is expired (exp outside leeway)".to_owned())
        }
        ErrorKind::ImmatureSignature => {
            AuthError::InvalidToken("token is not yet valid (nbf outside leeway)".to_owned())
        }
        ErrorKind::InvalidSignature => {
            AuthError::InvalidToken("token signature did not verify".to_owned())
        }
        ErrorKind::MissingRequiredClaim(claim) => {
            AuthError::InvalidToken(format!("token is missing the required claim {claim}"))
        }
        _ => AuthError::InvalidToken("token is malformed or could not be validated".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{build_validation, rsa_algorithm};
    use jsonwebtoken::Algorithm;

    #[test]
    fn maps_only_the_rsa_family() {
        assert_eq!(rsa_algorithm("RS256").unwrap(), Algorithm::RS256);
        assert_eq!(rsa_algorithm("RS384").unwrap(), Algorithm::RS384);
        assert_eq!(rsa_algorithm("RS512").unwrap(), Algorithm::RS512);
        assert!(rsa_algorithm("HS256").is_err());
        assert!(rsa_algorithm("none").is_err());
    }

    #[test]
    fn validation_pins_the_allowed_algorithms_and_is_never_empty() {
        let validation =
            build_validation("https://iss.example/", "aud", &["RS256".to_owned()], 60).unwrap();
        assert_eq!(validation.algorithms, vec![Algorithm::RS256]);
        assert!(validation.validate_exp);
        assert!(validation.validate_nbf);
        assert!(validation.validate_aud);
        assert_eq!(validation.leeway, 60);
    }

    #[test]
    fn an_empty_allowed_set_is_rejected() {
        assert!(build_validation("https://iss.example/", "aud", &[], 60).is_err());
    }

    #[test]
    fn issuer_is_pinned_verbatim_for_auth0_and_entra_shapes() {
        // Auth0 issuers carry a trailing slash; Entra v2 issuers do not. Neither is normalized,
        // so build_validation pins each verbatim and a slash-flipped variant is a different
        // issuer the token can never match.
        let auth0 = "https://tenant.us.auth0.com/";
        let entra = "https://login.microsoftonline.com/9188040d-6c67-4c5b-b112-36a304b66dad/v2.0";

        let iss_auth0 = build_validation(auth0, "aud", &["RS256".to_owned()], 0)
            .unwrap()
            .iss
            .expect("issuer is set");
        assert!(iss_auth0.contains(auth0));
        assert!(
            !iss_auth0.contains(auth0.trim_end_matches('/')),
            "the no-slash variant of an Auth0 issuer must not be accepted"
        );

        let iss_entra = build_validation(entra, "aud", &["RS256".to_owned()], 0)
            .unwrap()
            .iss
            .expect("issuer is set");
        assert!(iss_entra.contains(entra));
        assert!(
            !iss_entra.contains(&format!("{entra}/")),
            "a trailing slash appended to an Entra issuer must not be accepted"
        );
    }
}
