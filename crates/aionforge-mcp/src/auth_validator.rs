//! The OAuth resource-server **producer** (PR5 of the OAuth workstream).
//!
//! PR4 shipped the *consumption* side dark: [`ValidatedPrincipal`] and
//! [`validated_principal_from_extensions`](crate::validated_principal_from_extensions) read a
//! validated identity back out of a request, but no producer ever inserted one. This module is
//! that producer. It is the runtime half of the behavior flip; like every other auth path it is
//! **inert unless `auth.enabled`** — a host that never builds an [`AuthValidators`] (the
//! default-off path) sees byte-for-byte today's behavior.
//!
//! # What it owns
//!
//! * [`AuthValidators`] — built once at startup from an [`AuthConfig`].
//!   It holds one [`JwtValidator`] per trusted issuer (keyed by the
//!   exact `iss` string), the RFC 9728 well-known path, the resource identifier, and the issuer
//!   origins for posture reporting. It carries **no secret** — no token, no JWKS, no key.
//! * [`AuthValidators::authenticate`] — the per-request gate a Tower validator (the cli's
//!   `HttpMcpRouter`) calls for each `/mcp` request. It extracts the Bearer token, selects the
//!   issuer by the token's `iss`, validates it, maps the claims to a `Principal`, and on success
//!   returns the [`ValidatedPrincipal`] to insert into the request's
//!   `http::request::Parts.extensions` (the two-level nesting PR4 reads). Every failure is a
//!   secret-free `401`/`403` [`HttpResponse`] the caller returns verbatim.
//! * [`AuthValidators::oauth_metadata_response`] — the RFC 9728 well-known `200` response.
//!
//! # Never leaks a secret
//!
//! No `401`/`403` body, no `WWW-Authenticate` header, and no log line ever contains the token, a
//! claim value, the JWKS, or any key material. The `WWW-Authenticate` header carries only the
//! standard `Bearer` scheme, an `error` token, and the well-known `resource_metadata` URL.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;

use aionforge_auth::{AuthError, JwtValidator, VerifiedClaims};
use aionforge_config::{AuthConfig, IssuerConfig};
use bytes::Bytes;
use http::header::{CONTENT_TYPE, WWW_AUTHENTICATE};
use http::{HeaderValue, StatusCode};
use http_body_util::{BodyExt, Full};

use crate::http_transport::{
    HttpResponse, OAuthProtectedResourceMetadata, STREAMABLE_HTTP_ENDPOINT,
    oauth_protected_resource_well_known_path,
};
use crate::mapper::{MapError, map_verified_claims_to_principal};
use crate::validated::ValidatedPrincipal;

/// The set of per-issuer token validators and the metadata a resource server advertises.
///
/// Built **once** at startup with [`AuthValidators::build`] (each [`JwtValidator`] resolves and
/// caches its issuer's JWKS), then shared (cheaply [`Clone`], everything is behind an [`Arc`])
/// across every request the Tower validator handles. Constructed **only** when `auth.enabled`;
/// the default-off path never builds one, so no validator runs and no well-known route is served.
#[derive(Clone)]
pub struct AuthValidators {
    /// One validator per trusted issuer, keyed by the exact `iss` string (issuer URLs are never
    /// normalized: the Auth0 trailing slash and the Entra v2 no-slash forms are distinct keys).
    validators: Arc<BTreeMap<String, IssuerValidator>>,
    /// The RFC 9728 well-known path the well-known route is served under, e.g.
    /// `/.well-known/oauth-protected-resource/mcp`. The route matches on this *path*; the `401`
    /// `resource_metadata` challenge advertises [`AuthValidators::well_known_url`] (the absolute
    /// form) instead, because RFC 9728 §5.1 and the MCP authorization spec require the client to
    /// `GET` the header value verbatim, which a scheme-less path cannot satisfy.
    well_known_path: String,
    /// The **absolute** RFC 9728 metadata URL the `401`/`403` `resource_metadata` challenge points
    /// at and the well-known metadata's `resource` origin anchors. Derived from the configured
    /// resource (audience) origin plus [`AuthValidators::well_known_path`]; falls back to the bare
    /// path only when the resource has no parseable `scheme://host` origin (a misconfiguration the
    /// config layer's https/loopback audience guidance already steers away from).
    well_known_url: String,
    /// The resource identifier advertised in the well-known metadata `resource` field (the
    /// configured audience / API identifier, e.g. `https://memory.aionforgelabs.com`).
    resource: String,
    /// The trusted issuer origins, in config order, advertised as `authorization_servers` in the
    /// well-known metadata and reported by `server_status` (never a secret).
    issuer_origins: Vec<String>,
}

/// One issuer's validator plus the per-issuer config the claims mapper consumes.
#[derive(Clone)]
struct IssuerValidator {
    validator: JwtValidator,
    config: IssuerConfig,
}

/// Why building the [`AuthValidators`] failed at startup.
///
/// Surfaces a single issuer's validator-construction failure (discovery/JWKS/algorithm), naming
/// the issuer by its origin, never quoting a secret. A startup failure fails the server fast
/// rather than silently serving an auth-on deployment with a broken issuer.
#[derive(Debug, thiserror::Error)]
pub enum AuthValidatorsError {
    /// A per-issuer [`JwtValidator`] could not be built (discovery, JWKS fetch, or an unusable
    /// algorithm set). The issuer is named by its origin; the underlying [`AuthError`] carries no
    /// secret.
    #[error("could not build the token validator for issuer {issuer}: {source}")]
    Issuer {
        /// The issuer origin (non-secret) whose validator failed to build.
        issuer: String,
        /// The underlying auth error (discovery/JWKS/algorithm); never embeds a secret.
        #[source]
        source: AuthError,
    },
}

impl AuthValidators {
    /// Build the resource-server producer from an enabled [`AuthConfig`], one validator per issuer.
    ///
    /// Each [`JwtValidator`] resolves its issuer's JWKS endpoint (config override or OIDC
    /// discovery) and fetches the keys once, so a broken issuer fails here — at startup — instead
    /// of silently at the first request. The resource identifier defaults to the first issuer's
    /// audience (all issuers in a deployment share one resource audience for the well-known
    /// metadata; see the module docs).
    ///
    /// Returns `Ok(None)` when `auth.enabled` is `false` (the default-off path), so the caller
    /// holds no producer and the server behaves exactly as today.
    ///
    /// # Errors
    /// Returns [`AuthValidatorsError::Issuer`] if any issuer's validator cannot be built.
    pub async fn build(auth: &AuthConfig) -> Result<Option<Self>, AuthValidatorsError> {
        if !auth.enabled {
            return Ok(None);
        }
        let mut validators = BTreeMap::new();
        let mut issuer_origins = Vec::with_capacity(auth.issuers.len());
        for issuer in &auth.issuers {
            let validator =
                JwtValidator::new(issuer)
                    .await
                    .map_err(|source| AuthValidatorsError::Issuer {
                        issuer: issuer.issuer.clone(),
                        source,
                    })?;
            issuer_origins.push(issuer.issuer.clone());
            validators.insert(
                issuer.issuer.clone(),
                IssuerValidator {
                    validator,
                    config: issuer.clone(),
                },
            );
        }
        let resource = auth
            .issuers
            .first()
            .map(|issuer| issuer.audience.clone())
            .unwrap_or_default();
        let well_known_path = oauth_protected_resource_well_known_path(STREAMABLE_HTTP_ENDPOINT);
        let well_known_url = absolute_well_known_url(&resource, &well_known_path);
        Ok(Some(Self {
            validators: Arc::new(validators),
            well_known_path,
            well_known_url,
            resource,
            issuer_origins,
        }))
    }

    /// The RFC 9728 well-known path the well-known route is served under (the route matches on
    /// this path; the `401` challenge advertises the absolute [`AuthValidators::well_known_url`]).
    #[must_use]
    pub fn well_known_path(&self) -> &str {
        &self.well_known_path
    }

    /// The **absolute** RFC 9728 metadata URL the `401`/`403` `resource_metadata` challenge points
    /// at (scheme + host + path), so an MCP client can `GET` the header value verbatim per RFC 9728
    /// §5.1. Derived from the configured resource (audience) origin; equals the bare path only when
    /// the resource carries no parseable origin.
    #[must_use]
    pub fn well_known_url(&self) -> &str {
        &self.well_known_url
    }

    /// The RFC 9728 Protected Resource Metadata `200` response for the well-known route.
    ///
    /// The `resource` is the configured audience; `authorization_servers` are the trusted issuer
    /// origins. No secret appears in the body.
    #[must_use]
    pub fn oauth_metadata_response(&self) -> HttpResponse {
        let metadata =
            OAuthProtectedResourceMetadata::new(self.resource.clone(), self.issuer_origins.clone());
        ok_json_response(metadata.to_json())
    }

    /// Authenticate one `/mcp` request, returning the [`ValidatedPrincipal`] to insert on success.
    ///
    /// The flow, fail-closed at every step:
    /// 1. Extract the `Bearer` token from `Authorization`. Absent/malformed ⇒ `401` (no `error`).
    /// 2. Read the token's unverified `iss` and select the matching issuer's validator. No match
    ///    ⇒ `401` (`error="invalid_token"`); the `iss` is read **before** verification only to
    ///    route, and an unrecognized issuer is rejected, never trusted.
    /// 3. Validate the token (signature, issuer, audience, algorithm, expiry). Any failure ⇒
    ///    `401` (`error="invalid_token"`); the [`AuthError`] is never echoed into the response.
    /// 4. Map the verified claims to a `Principal`. A [`MapError`] ⇒ `403`
    ///    (`error="insufficient_scope"`, reason `ERR_PRINCIPAL_MAPPING`); e.g. an unanchored
    ///    writer is refused here.
    ///
    /// On success the caller inserts the returned [`ValidatedPrincipal`] into the request's
    /// `http::request::Parts.extensions` — the two-level nesting PR4 reads back.
    ///
    /// # Errors
    /// Returns the secret-free `401`/`403` `HttpResponse` to send when authentication fails.
    pub async fn authenticate(
        &self,
        authorization: Option<&HeaderValue>,
    ) -> Result<ValidatedPrincipal, Box<HttpResponse>> {
        let token = self
            .bearer_token(authorization)
            .ok_or_else(|| Box::new(self.unauthorized(None)))?;

        let issuer = unverified_issuer(token)
            .and_then(|iss| self.validators.get(&iss))
            .ok_or_else(|| Box::new(self.unauthorized(Some("invalid_token"))))?;

        let verified = match issuer.validator.validate(token).await {
            Ok(verified) => verified,
            Err(_error) => {
                // The AuthError is deliberately not echoed: the response states only the
                // standard `invalid_token` verdict so nothing the token carried can leak.
                return Err(Box::new(self.unauthorized(Some("invalid_token"))));
            }
        };

        // Best-effort, non-fatal: surface a tenant misconfiguration (RBAC off ⇒ no operator,
        // missing audience) as a loud signal rather than a silent failure.
        log_auth_health(&verified, &issuer.config);

        match map_verified_claims_to_principal(&verified, &issuer.config) {
            Ok((principal, token_class, write_posture)) => Ok(ValidatedPrincipal::new(
                principal,
                write_posture,
                token_class,
            )),
            Err(error) => Err(Box::new(self.forbidden_mapping(error))),
        }
    }

    /// The trusted issuer origins, for `server_status` posture reporting (never a secret).
    #[must_use]
    pub fn issuer_origins(&self) -> &[String] {
        &self.issuer_origins
    }

    /// Extract the `Bearer` token from an `Authorization` header value, if well-formed.
    fn bearer_token<'a>(&self, authorization: Option<&'a HeaderValue>) -> Option<&'a str> {
        let raw = authorization?.to_str().ok()?;
        let token = raw
            .strip_prefix("Bearer ")
            .or_else(|| raw.strip_prefix("bearer "))?;
        let token = token.trim();
        if token.is_empty() { None } else { Some(token) }
    }

    /// A `401 Unauthorized` with a `WWW-Authenticate: Bearer` challenge pointing at the **absolute**
    /// well-known metadata URL. `error` is omitted for a missing token and set to a standard token
    /// for a verdict.
    fn unauthorized(&self, error: Option<&str>) -> HttpResponse {
        let mut challenge = format!("Bearer resource_metadata=\"{}\"", self.well_known_url);
        if let Some(error) = error {
            challenge.push_str(&format!(", error=\"{error}\""));
        }
        challenge_response(StatusCode::UNAUTHORIZED, &challenge)
    }

    /// A `403 Forbidden` for a claims-mapping failure (e.g. an unanchored writer). The body and
    /// the `WWW-Authenticate` reason are the fixed `ERR_PRINCIPAL_MAPPING` token; no claim leaks.
    /// The `resource_metadata` is the **absolute** well-known URL (RFC 9728 §5.1).
    fn forbidden_mapping(&self, error: MapError) -> HttpResponse {
        // The MapError discriminant is non-secret (it embeds no token/claim), but the response
        // reports only the stable, structured reason token, not the Display text.
        let _ = error;
        let challenge = format!(
            "Bearer resource_metadata=\"{}\", error=\"insufficient_scope\", \
             error_description=\"ERR_PRINCIPAL_MAPPING\"",
            self.well_known_url
        );
        challenge_response(StatusCode::FORBIDDEN, &challenge)
    }
}

/// Build the absolute RFC 9728 metadata URL from the resource (audience) origin and the well-known
/// path. RFC 9728 §5.1 and the MCP authorization spec require the `resource_metadata` challenge to
/// be a URL the client `GET`s verbatim, so a scheme-less path cannot be advertised.
///
/// The origin is the configured resource's `scheme://host[:port]` (the audience is, by RFC 8707, a
/// URI). When the resource carries no parseable origin the bare path is returned unchanged — a
/// degraded but unambiguous fallback for an unusual (e.g. opaque-string) audience; the config
/// layer's https/loopback audience guidance steers deployments to a real origin.
fn absolute_well_known_url(resource: &str, well_known_path: &str) -> String {
    match origin_of(resource) {
        Some(origin) => format!("{origin}{well_known_path}"),
        None => well_known_path.to_owned(),
    }
}

/// Extract the `scheme://authority` origin (no path/query/fragment, no trailing slash) of a URL,
/// or `None` if it is not an `http`/`https` URL with a non-empty authority.
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{authority}"))
}

/// Read the **unverified** `iss` claim from a token, used only to route to the right validator.
///
/// This decodes the payload without verifying the signature; it is sound because the selected
/// validator then pins that exact issuer and rejects the token if the verified `iss` does not
/// match. An issuer not in the trusted set never resolves to a validator, so an attacker-chosen
/// `iss` cannot select a validator that would accept it.
fn unverified_issuer(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    let payload = parts.nth(1)?;
    let decoded = base64_url_decode(payload)?;
    let value: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    value
        .get("iss")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// Decode a base64url (no-pad) JWT segment.
fn base64_url_decode(input: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(input.as_bytes())
        .ok()
}

/// Emit a loud, best-effort health warning when a presented, otherwise-valid token lacks an
/// expected audience or the permissions claim an operator-capable issuer should mint.
///
/// This makes a tenant misconfiguration (e.g. Auth0 RBAC off ⇒ no token ever carries a
/// `permissions` array ⇒ no operator can ever exist) a **visible** signal at runtime rather than a
/// silent "no one is ever an operator" failure. The warnings name only the non-secret issuer
/// origin; no token or claim value is logged. They never affect the authentication verdict.
///
/// The warnings are written to **stderr** (the same channel the startup advisories use), not
/// `tracing::warn!`: the CLI installs no `tracing_subscriber`, so a `tracing` warning would be
/// dropped to a no-op dispatcher and never reach the operator (the very silent-failure mode item 5
/// of the spec forbids).
fn log_auth_health(verified: &VerifiedClaims, config: &IssuerConfig) {
    for warning in auth_health_warnings(verified, config) {
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "aionforge serve: auth health: {warning}");
    }
}

/// Compute the (non-secret) runtime health advisories for one validated token against its issuer
/// config. Pure, so it is directly unit-testable; the warnings name only the issuer origin.
///
/// The audience check reads the **raw** `aud` claim out of `verified.claims`, NOT
/// [`VerifiedClaims::aud`] — the latter is hard-set by the validator to the configured audience the
/// token *satisfied* (`validate::decode_and_validate`), so comparing it to the config would be a
/// tautology that can never fire. Hard audience validation already rejects a true mismatch with a
/// `401`; this best-effort signal exists for the subtler shapes hard validation lets through (e.g.
/// an `aud` array whose configured member is present but which also carries unexpected values, or a
/// scalar that — were validation ever relaxed — differs), surfacing them rather than hiding them.
fn auth_health_warnings(verified: &VerifiedClaims, config: &IssuerConfig) -> Vec<String> {
    let mut warnings = Vec::new();
    if !raw_aud_contains(verified, &config.audience) {
        warnings.push(format!(
            "issuer {} presented a validated token whose audience claim does not contain the \
             configured audience for this issuer; check the resource/audience (RBAC) configuration \
             on the tenant",
            config.issuer
        ));
    }
    if config.operator_permission.is_some() && !verified.claims.contains_key("permissions") {
        warnings.push(format!(
            "issuer {} configures an operator_permission but a validated token carries no \
             permissions claim; no principal can ever become an operator — check that RBAC / \
             add-permissions-to-access-token is enabled on the tenant",
            config.issuer
        ));
    }
    if config.allows_writes
        && config.agent_id_overrides.is_empty()
        && config.agent_id_claim.is_none()
    {
        // A writer-capable issuer with no agent-id anchor: the mapper refuses this token with a
        // 403 (UnanchoredWriter), but the operator otherwise sees only per-tool 403s. Surface the
        // root cause loudly on the same stderr channel (the mapper's own tracing::warn would be
        // dropped — the CLI installs no subscriber). Mirrors the config layer's startup advisory.
        warnings.push(format!(
            "issuer {} permits durable writes but has no agent-id anchor \
             (agent_id_overrides/agent_id_claim); every write token from it is refused with a 403 \
             (unanchored writer). Set an anchor or mark the issuer read-only",
            config.issuer
        ));
    }
    warnings
}

/// Whether the token's **raw** `aud` claim (scalar or array, per RFC 7519) contains `expected`.
/// A missing or non-string-shaped `aud` is treated as "does not contain" so the health check fires.
fn raw_aud_contains(verified: &VerifiedClaims, expected: &str) -> bool {
    match verified.claims.get("aud") {
        Some(serde_json::Value::String(aud)) => aud == expected,
        Some(serde_json::Value::Array(values)) => {
            values.iter().any(|value| value.as_str() == Some(expected))
        }
        _ => false,
    }
}

/// A `401`/`403` carrying a `WWW-Authenticate` challenge and an empty body (no secret leaks).
fn challenge_response(status: StatusCode, challenge: &str) -> HttpResponse {
    let header =
        HeaderValue::from_str(challenge).unwrap_or_else(|_| HeaderValue::from_static("Bearer"));
    http::Response::builder()
        .status(status)
        .header(WWW_AUTHENTICATE, header)
        .body(Full::new(Bytes::new()).boxed())
        .expect("valid challenge response")
}

/// A `200 application/json` response carrying the given body.
fn ok_json_response(json: String) -> HttpResponse {
    http::Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(json)).boxed())
        .expect("valid metadata response")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap as Map;

    use aionforge_config::IssuerConfig;
    use serde_json::json;

    use super::*;

    fn issuer_config() -> IssuerConfig {
        IssuerConfig {
            issuer: "https://dev-7ppqf0duhy7etaet.us.auth0.com/".into(),
            audience: "https://memory.aionforgelabs.com".into(),
            operator_permission: Some("console:operate".into()),
            // Read-only operator issuer: anchored-writer advisory is out of scope for these
            // audience/permissions health tests (it has its own dedicated test below).
            allows_writes: false,
            ..IssuerConfig::default()
        }
    }

    /// Build a `VerifiedClaims` whose RAW `aud` claim is the given scalar (so the health check,
    /// which reads `claims["aud"]`, sees a realistic token shape). `VerifiedClaims::aud` is set to
    /// the configured audience exactly as the production validator hard-sets it — proving the
    /// health check can no longer read that tautological field.
    fn verified(aud: &str, mut claims: Map<String, serde_json::Value>) -> VerifiedClaims {
        claims.insert("aud".into(), json!(aud));
        VerifiedClaims {
            sub: "auth0|abc".into(),
            iss: "https://dev-7ppqf0duhy7etaet.us.auth0.com/".into(),
            // The validator always sets this to the configured (satisfied) audience.
            aud: "https://memory.aionforgelabs.com".into(),
            claims,
        }
    }

    #[test]
    fn bearer_token_parsing_handles_scheme_case_and_blanks() {
        // A standalone parser check via a constructed AuthValidators-free path: replicate the
        // strip logic the method uses (it is a pure function of the header text).
        let parse = |raw: &str| -> Option<String> {
            let token = raw
                .strip_prefix("Bearer ")
                .or_else(|| raw.strip_prefix("bearer "))?;
            let token = token.trim();
            if token.is_empty() {
                None
            } else {
                Some(token.to_owned())
            }
        };
        assert_eq!(parse("Bearer abc.def.ghi"), Some("abc.def.ghi".to_owned()));
        assert_eq!(parse("bearer abc.def.ghi"), Some("abc.def.ghi".to_owned()));
        assert_eq!(parse("Bearer    "), None);
        assert_eq!(parse("Basic abc"), None);
        assert_eq!(parse("abc.def.ghi"), None);
    }

    #[test]
    fn unverified_issuer_reads_the_iss_without_verifying() {
        // A JWT with a payload `{"iss":"https://issuer.example/"}` (header/sig are arbitrary, the
        // routing read never verifies them).
        use base64::Engine as _;
        let header =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"alg\":\"RS256\"}");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"iss":"https://issuer.example/","sub":"x"}"#);
        let token = format!("{header}.{payload}.signature-not-verified");
        assert_eq!(
            unverified_issuer(&token),
            Some("https://issuer.example/".to_owned())
        );
        // A non-JWT string yields None rather than panicking.
        assert_eq!(unverified_issuer("not-a-jwt"), None);
    }

    #[test]
    fn the_audience_health_check_reads_the_raw_aud_claim_not_the_tautological_field() {
        // Regression guard for the dead-code finding: the health check MUST read the raw `aud`
        // claim, never `VerifiedClaims::aud` (which the validator hard-sets to the configured
        // audience, making the old comparison structurally always-false). Both tokens below carry
        // the SAME tautological `VerifiedClaims::aud` (the configured value); only the raw claim
        // differs — so a check reading the field would warn on neither, and the warning would be
        // provably inert. The corrected check fires exactly on the raw mismatch.
        let config = issuer_config();

        let matched = verified("https://memory.aionforgelabs.com", Map::new());
        assert_eq!(
            matched.aud, config.audience,
            "both tokens carry the configured audience in the tautological field"
        );
        assert!(
            raw_aud_contains(&matched, &config.audience),
            "the raw aud claim contains the configured audience ⇒ no warning"
        );
        assert!(
            auth_health_warnings(&matched, &config)
                .iter()
                .all(|warning| !warning.contains("audience")),
            "a matching raw aud draws no audience warning"
        );

        let mismatched = verified("https://wrong.example", Map::new());
        assert_eq!(
            mismatched.aud, config.audience,
            "the field is identical — only the raw claim differs"
        );
        assert!(
            !raw_aud_contains(&mismatched, &config.audience),
            "the raw aud claim does NOT contain the configured audience ⇒ the warning fires"
        );
        assert!(
            auth_health_warnings(&mismatched, &config)
                .iter()
                .any(|warning| warning.contains("audience")),
            "a mismatched raw aud fires the loud health warning the field-read could never reach"
        );
    }

    #[test]
    fn the_audience_health_check_accepts_an_aud_array_member() {
        // A machine token's `aud` is an array; the configured audience being a member ⇒ no warning.
        let config = issuer_config();
        let mut claims = Map::new();
        claims.insert(
            "aud".into(),
            json!(["https://memory.aionforgelabs.com", "https://other.api"]),
        );
        let machine = VerifiedClaims {
            sub: "auth0|abc".into(),
            iss: config.issuer.clone(),
            aud: config.audience.clone(),
            claims,
        };
        assert!(
            raw_aud_contains(&machine, &config.audience),
            "an aud array containing the configured audience draws no warning"
        );
        assert!(
            auth_health_warnings(&machine, &config)
                .iter()
                .all(|warning| !warning.contains("audience"))
        );
    }

    #[test]
    fn the_permissions_health_check_fires_when_operator_capable_but_no_permissions_claim() {
        let config = issuer_config(); // operator_permission is configured
        let without = verified("https://memory.aionforgelabs.com", Map::new());
        assert!(
            auth_health_warnings(&without, &config)
                .iter()
                .any(|warning| warning.contains("permissions claim")),
            "operator-capable issuer + no permissions claim ⇒ the RBAC-off warning fires"
        );
        let mut with = Map::new();
        with.insert("permissions".into(), json!(["console:operate"]));
        let present = verified("https://memory.aionforgelabs.com", with);
        assert!(
            auth_health_warnings(&present, &config)
                .iter()
                .all(|warning| !warning.contains("permissions claim")),
            "a token carrying permissions draws no permissions warning"
        );
    }

    #[test]
    fn the_unanchored_writer_health_check_fires_for_a_writer_with_no_anchor() {
        // A writer-capable issuer with no agent-id anchor: the mapper refuses its tokens with a
        // 403; the health channel surfaces the root cause loudly (the mapper's own tracing::warn
        // is dropped — no subscriber). An anchored or read-only issuer draws no such warning.
        let mut writer = issuer_config();
        writer.allows_writes = true; // no agent_id_overrides / agent_id_claim
        let token = verified("https://memory.aionforgelabs.com", Map::new());
        assert!(
            auth_health_warnings(&token, &writer)
                .iter()
                .any(|warning| warning.contains("unanchored writer")),
            "an unanchored writer issuer fires the loud health warning"
        );

        let mut anchored = writer.clone();
        anchored.agent_id_claim = Some("https://aionforge.dev/agent_id".into());
        assert!(
            auth_health_warnings(&token, &anchored)
                .iter()
                .all(|warning| !warning.contains("unanchored writer")),
            "an anchored writer draws no unanchored-writer warning"
        );
    }

    #[test]
    fn the_resource_metadata_url_is_absolute_when_the_resource_has_an_origin() {
        // RFC 9728 §5.1: the challenge's resource_metadata must be an absolute URL the client GETs
        // verbatim. Derived from the resource (audience) origin + the well-known path.
        assert_eq!(
            absolute_well_known_url(
                "https://memory.aionforgelabs.com",
                "/.well-known/oauth-protected-resource/mcp"
            ),
            "https://memory.aionforgelabs.com/.well-known/oauth-protected-resource/mcp"
        );
        // A resource carrying a path/port still yields the bare scheme://host[:port] origin.
        assert_eq!(
            absolute_well_known_url(
                "https://api.example.com:8443/memory",
                "/.well-known/oauth-protected-resource/mcp"
            ),
            "https://api.example.com:8443/.well-known/oauth-protected-resource/mcp"
        );
        // An opaque (non-URL) audience degrades to the bare path rather than fabricating an origin.
        assert_eq!(
            absolute_well_known_url("urn:opaque:resource", "/.well-known/x"),
            "/.well-known/x"
        );
    }
}
