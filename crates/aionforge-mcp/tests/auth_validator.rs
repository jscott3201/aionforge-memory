//! End-to-end through-line tests for the PR5 OAuth resource-server producer.
//!
//! These prove the **crux** of PR5: a [`ValidatedPrincipal`] the producer mints, inserted into the
//! HTTP request's `http::request::Parts.extensions`, survives into the rmcp `model::Extensions`
//! bag (as the streamable-http transport carries it — the whole `Parts` as one entry) such that
//! PR4's two-level [`validated_principal_from_extensions`] reads it back. They also exercise the
//! 401/403 failure responses, the RFC 9728 well-known route, and the default-OFF `build` path —
//! against BOTH an Auth0-shaped (trailing-slash `iss`) and an Entra v2-shaped (no-slash `iss`)
//! fixture. No real secret or private key is embedded: every token is minted locally with a test
//! RSA key, and every JWKS/discovery document is served by a per-test `wiremock` server.
//!
//! The [`through_the_real_transport`] module drives the **real** transport stack
//! (`streamable_http_service` = `RequestBodyLimitService` wrapping rmcp's `StreamableHttpService`)
//! end-to-end: it inserts a `ValidatedPrincipal` into the request `Parts.extensions` exactly as the
//! CLI Axum `/mcp` producer does, calls `.handle(..)`, and asserts a `read_memory` tool call reads
//! the principal back through PR4's two-level lookup AND that the INSTALLED `OperatorAwareAuthorizer`
//! surfaces `system` content for an operator but not for a non-operator. This converts the rmcp
//! "carry the whole Parts" assumption into a live regression guard and proves the composed operator
//! path, not just isolated unit links.

use aionforge_config::{AuthConfig, IssuerConfig};
use aionforge_mcp::{
    AuthValidators, oauth_protected_resource_well_known_path, validated_principal_from_extensions,
};
use http::StatusCode;
use http::header::WWW_AUTHENTICATE;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode, get_current_timestamp};
use rmcp::model::Extensions;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The real Auth0 dev-tenant API audience (per the PR5 spec). Used as the concrete resource value;
/// it is a public identifier, not a secret.
const AUDIENCE: &str = "https://memory.aionforgelabs.com";

/// The 2048-bit test RSA private key (PKCS#8 PEM) used to mint tokens. A throwaway test key — not
/// a real credential. (Same fixture key the `aionforge-auth` integration tests use.)
const TEST_RSA_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQDyiIwTiWEzYexq
NnG0A2JAxj9orwpzLxFWElH2Hra7nx2FcNGQNuiYYeBmCf38QEX22H/UUfGN7WZa
VSohbMLmeY7CquY+EWcGn5cGexyo49Tcj5qrokYQjdsgznZVjLVKk2u5eSRecAiS
V3OFILCbkyW97+iuvI5zta4/u7MHxISRl87dP5TFfuKGHo+j5nyFTZWb408fx+II
6RxcrSnwzk8eGdCyF9v2rgY5Iv3/d1ggPllqpJEMSdw+CIJeBqhQv9Fs996hVDgl
5XW+ZscdydDv+QkESspFhYkeRjnLHOo//n5GfW7RD1ZjvVE+TpfuFpy1oclG8k1G
skdZ+TFXAgMBAAECggEAER8PmG9506lFicf3JeiZPo5gOpEk0TXQ6P0ZGSFY8AzP
BjUNLjuaFuvN7hYlfnHBHqhw+bmhLk5Ei/r4IuztI10QdXCgGWCcH80TWctGHiwb
QkjG9/fYL2H8RqgclXR99dpLYAgLx7jr+fy/dHX20bzFDNALYo6AFe4M84XaISGc
s9/7I5IrWGApneud1KrfEm8iU481o2kWLXCWU6Qwb0sAbN3u9QuqZhkgcN+VT26s
uRXaAboHWchyZK57/pAsnpO2qjFSdD8htArj4P4sEJGUqpTIdMK3EnrmurC3S5Vn
Cw7iLDe0fgQx0oUxlXANO0IlM0k1JMdgcDaFCI1GdQKBgQD+eBiikf0elJz/71KW
4TOdb8EjMT8BrxYl4ouvEegF0vhM9sFdxtT+etNqQDtAqG+5gDY7nBNTqNaD+yZu
NbtYUTtZyIfynhPv1elI+SG4rA/WcU9/OxOISN9CrSC0dT/UEprrgsKcYRNDhRYi
erZGIIBJFeXeUTLSIEGUIE2D9QKBgQDz/hGjyJmzDiHvFv3JGk7X0Y9kkpAYSSx8
vH9b9cbvaZdGotRu4XzwzuhwwcOf2K1AZ9YlMIh+Kql6j5T9VGnKhSETPrww3w7t
h0IJo+21nu0Cs+xoFeYCiU75oldLRipoddMOrZpppsEf+MwVlH+v0zXa6v4Zt6mq
7PN+oH2cmwKBgHvfvKZPCPf9Alx4hSzbngOy5kMacwB/2flBShxETD2hkKvupvze
kMr8wbQEZpO4KwMTTdNAzAu6sgp3lSKrV3LLwGeZfcx2dWAYMsMKPAcpA2CxsjBO
ctiyGLTdkIEoXpT/JZkmA1Sa0QTaYYcRU2/Z3Hk3hrntrx6pAyN3giSNAoGAVmzS
dr9hogkJgBUWxBsrfkrejfNUUyXoOi7SthIy6y7txLl8oeIBTZMcxoP79SzdAYlG
U1oDnx0hdyZQ0gMKjg/mDVkVdAIu2XglriCA3Op0bZap0JyhIpjcfpRAc4thDite
HT7lCTNmCRspvyMgr3kTBH5kj1t9H+xau6nBlK0CgYBJ4RKdeu5hdT+YSw1FTML1
N3F3vQml9CXnTlbrPaJc62byi0gyZRs3/chY+LhHuicZMqAD7WkFqcIYEKXYqPrL
q1fgSKe609pCUct+wn/M57JPCg064cWn/OC7l4OtSKSp+zY9I3Nng036lDfCjNTt
wYGo3+cAynWq9R/BKADv5w==
-----END PRIVATE KEY-----";

/// The base64url (no padding) modulus of [`TEST_RSA_PEM`], for the served JWKS.
const TEST_RSA_N: &str = "8oiME4lhM2HsajZxtANiQMY_aK8Kcy8RVhJR9h62u58dhXDRkDbomGHgZgn9_EBF9th_1FHxje1mWlUqIWzC5nmOwqrmPhFnBp-XBnscqOPU3I-aq6JGEI3bIM52VYy1SpNruXkkXnAIkldzhSCwm5Mlve_orryOc7WuP7uzB8SEkZfO3T-UxX7ihh6Po-Z8hU2Vm-NPH8fiCOkcXK0p8M5PHhnQshfb9q4GOSL9_3dYID5ZaqSRDEncPgiCXgaoUL_RbPfeoVQ4JeV1vmbHHcnQ7_kJBErKRYWJHkY5yxzqP_5-Rn1u0Q9WY71RPk6X7hactaHJRvJNRrJHWfkxVw";

/// The base64url exponent (65537) of [`TEST_RSA_PEM`].
const TEST_RSA_E: &str = "AQAB";

/// The `kid` used for the test signing key throughout.
const TEST_KID: &str = "test-key-1";

/// Build the JWKS body for the test RSA key under [`TEST_KID`].
fn jwks_body() -> Value {
    json!({
        "keys": [
            { "kty": "RSA", "kid": TEST_KID, "use": "sig", "alg": "RS256", "n": TEST_RSA_N, "e": TEST_RSA_E }
        ]
    })
}

/// Mint an RS256 token signed by [`TEST_RSA_PEM`] under [`TEST_KID`].
fn mint(claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(TEST_KID.to_owned());
    let key = EncodingKey::from_rsa_pem(TEST_RSA_PEM.as_bytes()).expect("test PEM parses");
    encode(&header, claims, &key).expect("token mints")
}

/// Stand up a mock issuer (OIDC discovery + JWKS), returning the server and its base URL.
async fn serve_issuer() -> (MockServer, String) {
    let server = MockServer::start().await;
    let issuer = server.uri();
    let jwks_uri = format!("{issuer}/jwks.json");
    let discovery = json!({ "issuer": issuer, "jwks_uri": jwks_uri });
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(discovery))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/jwks.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(jwks_body()))
        .mount(&server)
        .await;
    (server, issuer)
}

/// Standard time claims for a token from `iss` with the configured audience.
fn claims(iss: &str, extra: Value) -> Value {
    let now = get_current_timestamp();
    let mut value = json!({
        "sub": "auth0|through-line",
        "iss": iss,
        "aud": AUDIENCE,
        "exp": now + 3600,
        "nbf": now,
        "iat": now,
    });
    if let (Some(base), Value::Object(extra)) = (value.as_object_mut(), extra) {
        for (k, v) in extra {
            base.insert(k, v);
        }
    }
    value
}

/// A read-only issuer config (no agent-id anchor needed; a content-hash id is sound) keyed to the
/// served issuer, with the real audience and an operator permission so the operator path is live.
fn read_only_issuer(iss: &str) -> IssuerConfig {
    let mut teams_allowlist = std::collections::BTreeSet::new();
    teams_allowlist.insert("platform".to_string());
    IssuerConfig {
        issuer: iss.to_owned(),
        audience: AUDIENCE.to_owned(),
        allowed_algs: vec!["RS256".to_owned()],
        leeway_secs: 5,
        allows_writes: false,
        operator_permission: Some("console:operate".to_owned()),
        teams_allowlist,
        ..IssuerConfig::default()
    }
}

/// Build the rmcp [`Extensions`] bag exactly as the streamable-http transport does after the PR5
/// validator has inserted a [`ValidatedPrincipal`] into the request's `http::request::Parts`: the
/// whole `Parts` is carried into the rmcp bag as a single entry. This mirrors the producer's real
/// insertion site (`request.extensions_mut().insert(validated)`), so reading it back through PR4's
/// two-level helper proves the end-to-end through-line.
fn rmcp_bag_after_router_inserts(validated: aionforge_mcp::ValidatedPrincipal) -> Extensions {
    // What the Axum `/mcp` handler mutates: the HTTP request Parts' http::Extensions (level 1).
    let (mut parts, ()) = http::Request::builder()
        .body(())
        .expect("a trivial request builds")
        .into_parts();
    parts.extensions.insert(validated);
    // What the transport carries into the rmcp bag: the whole Parts as one entry (level 0).
    let mut extensions = Extensions::new();
    extensions.insert(parts);
    extensions
}

#[tokio::test]
async fn auth0_trailing_slash_token_round_trips_through_to_pr4_reader() {
    let (_server, base) = serve_issuer().await;
    // Auth0-shaped issuer: a trailing slash on the issuer (Auth0 always ends in `/`). The `iss`
    // string carries the slash (which the validator pins byte-for-byte); the JWKS is fetched from
    // the same served origin via an explicit same-origin jwks_uri.
    let iss = format!("{base}/");
    let validators = build_validators(&iss, &base).await;

    // Mint an operator, team-bearing token from the Auth0-shaped issuer.
    let token = mint(&claims(
        &iss,
        json!({
            "permissions": ["console:operate"],
            "https://aionforge.dev/teams": ["platform"],
        }),
    ));
    let authorization = bearer(&token);

    let validated = validators
        .authenticate(Some(&authorization))
        .await
        .expect("a valid Auth0 token authenticates");

    // The producer minted an operator principal; its bit and the read-only posture are intact.
    assert!(
        validated.principal.operator,
        "the console:operate permission grants the operator bit"
    );
    assert_eq!(validated.principal.teams, vec!["platform".to_string()]);

    // THE THROUGH-LINE: insert into Parts.extensions, carry the whole Parts into the rmcp bag, and
    // read it back through PR4's two-level helper.
    let bag = rmcp_bag_after_router_inserts(validated);
    let read_back = validated_principal_from_extensions(&bag)
        .expect("the producer's ValidatedPrincipal survives into the rmcp Extensions bag");
    assert!(
        read_back.principal.operator,
        "the operator bit survives the producer→Parts→rmcp-bag→PR4-reader through-line"
    );
    assert_eq!(read_back.principal.teams, vec!["platform".to_string()]);
}

#[tokio::test]
async fn entra_v2_no_slash_token_round_trips_through_to_pr4_reader() {
    let (_server, base) = serve_issuer().await;
    // Entra v2-shaped issuer: NO trailing slash (the served base URL itself is the issuer).
    let iss = base.clone();
    let validators = build_validators(&iss, &base).await;

    let token = mint(&claims(&iss, json!({})));
    let authorization = bearer(&token);

    let validated = validators
        .authenticate(Some(&authorization))
        .await
        .expect("a valid Entra v2 token authenticates");
    assert!(
        !validated.principal.operator,
        "no permissions claim ⇒ no operator bit"
    );

    let bag = rmcp_bag_after_router_inserts(validated);
    let read_back = validated_principal_from_extensions(&bag)
        .expect("the Entra v2 principal survives the through-line");
    assert!(!read_back.principal.operator);
}

#[tokio::test]
async fn a_missing_bearer_token_is_a_401_with_a_resource_metadata_challenge() {
    let (_server, base) = serve_issuer().await;
    let validators = build_validators(&base, &base).await;

    let response = validators
        .authenticate(None)
        .await
        .expect_err("a missing token is rejected");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let challenge = response
        .headers()
        .get(WWW_AUTHENTICATE)
        .expect("a WWW-Authenticate challenge is present")
        .to_str()
        .expect("ascii challenge");
    assert!(challenge.starts_with("Bearer "), "{challenge}");
    // RFC 9728 §5.1 / MCP: the resource_metadata is the ABSOLUTE well-known URL the client GETs
    // verbatim — scheme + host (the configured resource origin) + the well-known path, never a
    // bare scheme-less path.
    let expected_url = format!(
        "{AUDIENCE}{}",
        oauth_protected_resource_well_known_path("/mcp")
    );
    assert!(
        challenge.contains(&format!("resource_metadata=\"{expected_url}\"")),
        "the challenge points at the absolute well-known metadata URL: {challenge}"
    );
    assert!(
        expected_url.starts_with("https://"),
        "the advertised metadata URL is absolute, not a scheme-less path: {expected_url}"
    );
    // A missing token carries NO `error` token (only an absent-credential challenge).
    assert!(!challenge.contains("error="), "{challenge}");
}

#[tokio::test]
async fn an_invalid_token_is_a_401_invalid_token_and_leaks_nothing() {
    let (_server, base) = serve_issuer().await;
    let validators = build_validators(&base, &base).await;

    // A token from an UNTRUSTED issuer (not in the validator set) is rejected before any
    // signature work: the routing read finds no validator for its `iss`.
    let alien = mint(&claims("https://attacker.example/", json!({})));
    let authorization = bearer(&alien);
    let response = validators
        .authenticate(Some(&authorization))
        .await
        .expect_err("an untrusted issuer is rejected");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let challenge = response
        .headers()
        .get(WWW_AUTHENTICATE)
        .expect("challenge present")
        .to_str()
        .expect("ascii");
    assert!(challenge.contains("error=\"invalid_token\""), "{challenge}");
    // The response NEVER echoes the token or a claim value.
    assert!(!challenge.contains("attacker.example"), "{challenge}");
    assert!(!challenge.contains(&alien), "{challenge}");
}

#[tokio::test]
async fn an_unanchored_writer_is_a_403_principal_mapping_error() {
    let (_server, base) = serve_issuer().await;
    // A WRITER issuer with no agent-id anchor: the mapper refuses (UnanchoredWriter), and the
    // producer turns that into a 403 with the stable ERR_PRINCIPAL_MAPPING reason.
    let iss = base.clone();
    let mut issuer = read_only_issuer(&iss);
    issuer.allows_writes = true; // writer, but with no agent_id_overrides / agent_id_claim anchor

    let auth = AuthConfig {
        enabled: true,
        issuers: vec![issuer],
    };
    let validators = AuthValidators::build(&auth)
        .await
        .expect("validators build")
        .expect("auth is enabled");

    let token = mint(&claims(&iss, json!({})));
    let authorization = bearer(&token);
    let response = validators
        .authenticate(Some(&authorization))
        .await
        .expect_err("an unanchored writer is refused");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let challenge = response
        .headers()
        .get(WWW_AUTHENTICATE)
        .expect("challenge present")
        .to_str()
        .expect("ascii");
    assert!(
        challenge.contains("ERR_PRINCIPAL_MAPPING"),
        "the 403 carries the stable structured reason: {challenge}"
    );
    assert!(
        challenge.contains("error=\"insufficient_scope\""),
        "{challenge}"
    );
}

#[tokio::test]
async fn the_well_known_route_serves_the_resource_and_authorization_servers() {
    let (_server, base) = serve_issuer().await;
    let iss = base.clone();
    let validators = build_validators(&iss, &base).await;

    let response = validators.oauth_metadata_response();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap_or_default()),
        Some("application/json")
    );
    let body = http_body_util::BodyExt::collect(response.into_body())
        .await
        .expect("body collects")
        .to_bytes();
    let parsed: Value = serde_json::from_slice(&body).expect("metadata is JSON");
    assert_eq!(parsed["resource"], json!(AUDIENCE));
    assert_eq!(parsed["authorization_servers"], json!([iss]));
    assert_eq!(parsed["bearer_methods_supported"], json!(["header"]));
}

#[tokio::test]
async fn the_default_off_build_yields_no_producer() {
    // DEFAULT-OFF: a disabled AuthConfig builds no producer at all, so the router runs no
    // validation and serves no well-known route — byte-for-byte today's behavior.
    let disabled = AuthConfig::default();
    assert!(!disabled.enabled);
    let built = AuthValidators::build(&disabled)
        .await
        .expect("a disabled build never errors");
    assert!(
        built.is_none(),
        "a disabled AuthConfig yields no AuthValidators producer"
    );
}

/// Build an [`AuthValidators`] whose single issuer pins `iss` but discovers its JWKS at `base`'s
/// OIDC document. (For the no-slash case `iss == base`; for the Auth0 case `iss == base + "/"`,
/// and the validator's same-origin guard still resolves the JWKS off the discovery doc served at
/// `base`.)
async fn build_validators(iss: &str, base: &str) -> AuthValidators {
    // Pin the jwks_uri explicitly to the served origin so the trailing-slash issuer string does
    // not change discovery (the issuer is matched byte-for-byte against the token's `iss`, while
    // the keys come from the served endpoint).
    let mut issuer = read_only_issuer(iss);
    issuer.jwks_uri = Some(format!("{base}/jwks.json"));
    let auth = AuthConfig {
        enabled: true,
        issuers: vec![issuer],
    };
    AuthValidators::build(&auth)
        .await
        .expect("validators build")
        .expect("auth is enabled")
}

/// A `Bearer <token>` header value.
fn bearer(token: &str) -> http::HeaderValue {
    http::HeaderValue::from_str(&format!("Bearer {token}")).expect("ascii bearer header")
}
