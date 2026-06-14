//! Integration tests for the RS256-pinned JWT validator.
//!
//! Every test mints its tokens locally with a fixed test RSA key and serves the OIDC
//! discovery + JWKS documents through a per-test `wiremock` server. There is no real network
//! traffic. The security tests assert that `alg=none`, `HS*` algorithm confusion, an issuer or
//! audience mismatch, an expired/not-yet-valid token, and an unknown `kid` are each rejected
//! with the matching [`AuthError`] verdict.

use std::collections::BTreeMap;

use aionforge_auth::{AuthError, JwtValidator};
use aionforge_config::IssuerConfig;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode, get_current_timestamp};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The 2048-bit test RSA private key (PKCS#8 PEM) used to mint tokens.
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

/// A *second* RSA key's modulus, used to serve a JWKS that does NOT match the signing key, so
/// a structurally valid token fails the signature check.
const OTHER_RSA_N: &str = "3TiBdm7qol3bqoqONxtYD4ljs3_LMO7sKpUMZjBehJDE8kOUatHXbbdZURRFRLjZ-nKoEPJ336eWfpEtJiSuc-WiMFWRa7waHwmhzYgkIIS3tdeMRHQXKTleEyO5dV6um4Kbaok8NBpz4S3_ctwQWSBGie7A8twP65N2SKYKwcRSYeeBIIi6YcgjcqTcPdhKOy0qyJEemRGoPCY77zTwJ49opgU4ySJ2Pkj3gg48BBZ6X2SEXQyoWEWP47EdafIW_-9-C-VhKi08YxD9BQvdWwmyV_a8qAnEgv5N4k4X3z2POMYlm3_KQTu5EijqsWXoIm4jW2V7JLLl-Zoz2VhMwQ";

/// The `kid` used for the test signing key throughout.
const TEST_KID: &str = "test-key-1";

/// Build the JWKS body for an RSA key under [`TEST_KID`].
fn jwks_for(n: &str) -> Value {
    json!({
        "keys": [
            { "kty": "RSA", "kid": TEST_KID, "use": "sig", "alg": "RS256", "n": n, "e": TEST_RSA_E }
        ]
    })
}

/// Mint an RS256 token signed by [`TEST_RSA_PEM`] under `kid`, with the given claims.
fn mint_token(kid: &str, claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.to_owned());
    let key = EncodingKey::from_rsa_pem(TEST_RSA_PEM.as_bytes()).expect("test PEM parses");
    encode(&header, claims, &key).expect("token mints")
}

/// Stand up a mock server serving an OIDC discovery document and a JWKS body, returning the
/// server (kept alive for the test) and the issuer string (the server's own base URL).
async fn serve(jwks_body: Value) -> (MockServer, String) {
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
        .respond_with(ResponseTemplate::new(200).set_body_json(jwks_body))
        .mount(&server)
        .await;
    (server, issuer)
}

/// A baseline issuer config pointing at a served issuer with the given audience.
fn config_for(issuer: &str, audience: &str) -> IssuerConfig {
    IssuerConfig {
        issuer: issuer.to_owned(),
        audience: audience.to_owned(),
        allowed_algs: vec!["RS256".to_owned()],
        leeway_secs: 5,
        ..IssuerConfig::default()
    }
}

/// Standard time claims: issued now, valid for an hour.
fn standard_claims(issuer: &str, audience: &str) -> Value {
    let now = get_current_timestamp();
    json!({
        "sub": "agent-123",
        "iss": issuer,
        "aud": audience,
        "exp": now + 3600,
        "nbf": now,
        "iat": now,
        "https://aionforge.dev/teams": ["alpha", "beta"]
    })
}

#[tokio::test]
async fn happy_path_rs256_token_validates() {
    let (_server, issuer) = serve(jwks_for(TEST_RSA_N)).await;
    let audience = "https://memory.aionforge.dev";
    let config = config_for(&issuer, audience);
    let validator = JwtValidator::new(&config).await.expect("validator builds");

    let token = mint_token(TEST_KID, &standard_claims(&issuer, audience));
    let claims = validator.validate(&token).await.expect("token validates");

    assert_eq!(claims.sub, "agent-123");
    assert_eq!(claims.iss, issuer);
    assert_eq!(claims.aud, audience);
    // The full raw map is available to the PR3 mapper, including the issuer-specific claim.
    assert_eq!(
        claims.claims.get("https://aionforge.dev/teams"),
        Some(&json!(["alpha", "beta"]))
    );
    let _: &BTreeMap<String, Value> = &claims.claims;
}

#[tokio::test]
async fn alg_none_is_rejected() {
    let (_server, issuer) = serve(jwks_for(TEST_RSA_N)).await;
    let audience = "aud-1";
    let config = config_for(&issuer, audience);
    let validator = JwtValidator::new(&config).await.expect("validator builds");

    // Hand-craft an alg=none token (header.alg = "none", empty signature).
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = b64.encode(json!({ "alg": "none", "typ": "JWT", "kid": TEST_KID }).to_string());
    let payload = b64.encode(standard_claims(&issuer, audience).to_string());
    let token = format!("{header}.{payload}.");

    let err = validator
        .validate(&token)
        .await
        .expect_err("alg=none is rejected");
    // `none` is not a jsonwebtoken Algorithm, so header decode fails before any key use and the
    // verdict is deterministically InvalidToken (never AlgorithmNotAllowed via a later path).
    assert!(
        matches!(err, AuthError::InvalidToken(_)),
        "alg=none must be rejected at header decode, got {err:?}"
    );
}

#[tokio::test]
async fn hs256_algorithm_confusion_is_rejected() {
    let (_server, issuer) = serve(jwks_for(TEST_RSA_N)).await;
    let audience = "aud-1";
    let config = config_for(&issuer, audience);
    let validator = JwtValidator::new(&config).await.expect("validator builds");

    // Sign an HS256 token using the RSA public-key bytes as the HMAC secret (the classic
    // confusion attack). The kid points at the RSA key in the JWKS.
    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some(TEST_KID.to_owned());
    let secret = EncodingKey::from_secret(TEST_RSA_N.as_bytes());
    let token =
        encode(&header, &standard_claims(&issuer, audience), &secret).expect("HS256 token mints");

    let err = validator
        .validate(&token)
        .await
        .expect_err("HS256 is rejected");
    assert!(
        matches!(err, AuthError::AlgorithmNotAllowed(_)),
        "HS256 must be refused by the RSA-only allow-list, got {err:?}"
    );
}

#[tokio::test]
async fn issuer_byte_match_auth0_vs_entra() {
    // Auth0-shaped issuer (the served issuer is the mock base URL; the claim must match it
    // byte-for-byte). We prove a token whose iss has an extra trailing slash is rejected.
    let (_server, issuer) = serve(jwks_for(TEST_RSA_N)).await;
    let audience = "aud-1";
    let config = config_for(&issuer, audience);
    let validator = JwtValidator::new(&config).await.expect("validator builds");

    // A token whose iss is the configured issuer with a trailing slash appended is a DIFFERENT
    // byte sequence and must be rejected (no normalization).
    let mut claims = standard_claims(&issuer, audience);
    claims["iss"] = json!(format!("{issuer}/"));
    let token = mint_token(TEST_KID, &claims);

    let err = validator
        .validate(&token)
        .await
        .expect_err("iss mismatch rejected");
    assert!(matches!(err, AuthError::IssuerMismatch), "got {err:?}");

    // Sanity: the exact issuer still passes.
    let good = mint_token(TEST_KID, &standard_claims(&issuer, audience));
    validator
        .validate(&good)
        .await
        .expect("exact issuer validates");
}

#[tokio::test]
async fn audience_mismatch_is_rejected() {
    let (_server, issuer) = serve(jwks_for(TEST_RSA_N)).await;
    let config = config_for(&issuer, "https://api.different.com");
    let validator = JwtValidator::new(&config).await.expect("validator builds");

    let token = mint_token(
        TEST_KID,
        &standard_claims(&issuer, "https://api.example.com"),
    );
    let err = validator
        .validate(&token)
        .await
        .expect_err("aud mismatch rejected");
    assert!(matches!(err, AuthError::AudienceMismatch), "got {err:?}");
}

#[tokio::test]
async fn expired_token_outside_leeway_is_rejected_but_inside_leeway_passes() {
    let (_server, issuer) = serve(jwks_for(TEST_RSA_N)).await;
    let audience = "aud-1";
    let mut config = config_for(&issuer, audience);
    config.leeway_secs = 5;
    let validator = JwtValidator::new(&config).await.expect("validator builds");

    let now = get_current_timestamp();
    // Expired by 30s, well outside the 5s leeway -> rejected.
    let mut claims = standard_claims(&issuer, audience);
    claims["exp"] = json!(now - 30);
    claims["nbf"] = json!(now - 60);
    claims["iat"] = json!(now - 60);
    let token = mint_token(TEST_KID, &claims);
    let err = validator
        .validate(&token)
        .await
        .expect_err("expired token rejected");
    assert!(matches!(err, AuthError::InvalidToken(_)), "got {err:?}");

    // Now expired by 2s with a 60s leeway -> accepted.
    let mut config2 = config_for(&issuer, audience);
    config2.leeway_secs = 60;
    let validator2 = JwtValidator::new(&config2).await.expect("validator builds");
    let mut claims2 = standard_claims(&issuer, audience);
    claims2["exp"] = json!(now - 2);
    let token2 = mint_token(TEST_KID, &claims2);
    validator2
        .validate(&token2)
        .await
        .expect("token within leeway validates");
}

#[tokio::test]
async fn not_yet_valid_token_outside_leeway_is_rejected() {
    let (_server, issuer) = serve(jwks_for(TEST_RSA_N)).await;
    let audience = "aud-1";
    let mut config = config_for(&issuer, audience);
    config.leeway_secs = 5;
    let validator = JwtValidator::new(&config).await.expect("validator builds");

    let now = get_current_timestamp();
    let mut claims = standard_claims(&issuer, audience);
    claims["nbf"] = json!(now + 60);
    let token = mint_token(TEST_KID, &claims);
    let err = validator
        .validate(&token)
        .await
        .expect_err("nbf in future rejected");
    assert!(matches!(err, AuthError::InvalidToken(_)), "got {err:?}");
}

#[tokio::test]
async fn unknown_kid_triggers_one_refetch_then_fails_closed() {
    // Serve a JWKS that only has TEST_KID; the token claims an unknown kid.
    let server = MockServer::start().await;
    let issuer = server.uri();
    let jwks_uri = format!("{issuer}/jwks.json");
    let discovery = json!({ "issuer": issuer, "jwks_uri": jwks_uri });
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(discovery))
        .mount(&server)
        .await;
    // The construction fetch + the single refetch are the only two JWKS hits we expect.
    Mock::given(method("GET"))
        .and(path("/jwks.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(jwks_for(TEST_RSA_N)))
        .expect(2)
        .mount(&server)
        .await;

    let audience = "aud-1";
    let config = config_for(&issuer, audience);
    let validator = JwtValidator::new(&config).await.expect("validator builds");

    let token = mint_token("unknown-key-id", &standard_claims(&issuer, audience));
    // First call: cache miss on the unknown kid -> ONE refetch -> still absent -> NoMatchingKey.
    let err = validator
        .validate(&token)
        .await
        .expect_err("unknown kid rejected");
    assert!(matches!(err, AuthError::NoMatchingKey(_)), "got {err:?}");

    // Second call with the same unknown kid: the cache already holds TEST_KID and not the
    // unknown one, so it still misses and refetches. To prove NO extra fetch on a *cached*
    // kid, validate a good token now and assert the JWKS endpoint was not hit a third time.
    let good = mint_token(TEST_KID, &standard_claims(&issuer, audience));
    validator
        .validate(&good)
        .await
        .expect("cached kid validates without refetch");
    // The `.expect(2)` on the mock asserts exactly two JWKS fetches total: construction + the
    // single unknown-kid refetch. The good-token call used the cache (no third fetch); wiremock
    // verifies the expectation when `server` drops at end of test.
    drop(server);
}

#[tokio::test]
async fn malformed_jwks_is_a_refresh_error() {
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
        .respond_with(ResponseTemplate::new(200).set_body_string("{ not valid json"))
        .mount(&server)
        .await;

    let config = config_for(&issuer, "aud-1");
    let err = JwtValidator::new(&config)
        .await
        .expect_err("malformed JWKS fails construction");
    assert!(matches!(err, AuthError::JwksRefresh(_)), "got {err:?}");
}

#[tokio::test]
async fn ec_key_in_jwks_for_rs256_token_yields_no_matching_key() {
    // Serve a JWKS that contains only an EC key (no usable RSA verify key). The RS256 token's
    // kid cannot be resolved, so after the single refetch we get NoMatchingKey.
    let ec_jwks = json!({
        "keys": [
            { "kty": "EC", "kid": "ec-1", "crv": "P-256", "x": "f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU", "y": "x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0" }
        ]
    });
    let (_server, issuer) = serve(ec_jwks).await;
    let audience = "aud-1";
    let config = config_for(&issuer, audience);
    let validator = JwtValidator::new(&config)
        .await
        .expect("validator builds with EC-only JWKS");

    let token = mint_token(TEST_KID, &standard_claims(&issuer, audience));
    let err = validator
        .validate(&token)
        .await
        .expect_err("RS256 token has no RSA key");
    assert!(matches!(err, AuthError::NoMatchingKey(_)), "got {err:?}");
}

#[tokio::test]
async fn wrong_signing_key_fails_signature_check() {
    // The JWKS publishes a DIFFERENT RSA modulus under TEST_KID, so the token (signed by
    // TEST_RSA_PEM) does not verify -> InvalidToken (bad signature), never a panic.
    let (_server, issuer) = serve(jwks_for(OTHER_RSA_N)).await;
    let audience = "aud-1";
    let config = config_for(&issuer, audience);
    let validator = JwtValidator::new(&config).await.expect("validator builds");

    let token = mint_token(TEST_KID, &standard_claims(&issuer, audience));
    let err = validator
        .validate(&token)
        .await
        .expect_err("wrong key rejects signature");
    assert!(matches!(err, AuthError::InvalidToken(_)), "got {err:?}");
}

#[tokio::test]
async fn discovery_unreachable_is_a_discovery_error() {
    // Point at a closed port (no server). Discovery must fail closed with a Discovery error.
    let config = config_for("http://127.0.0.1:1", "aud-1");
    let err = JwtValidator::new(&config)
        .await
        .expect_err("unreachable discovery fails");
    assert!(matches!(err, AuthError::Discovery(_)), "got {err:?}");
}

#[tokio::test]
async fn config_jwks_uri_on_a_foreign_origin_is_refused_pre_fetch() {
    // SSRF guard, config-override path: the issuer is the (loopback) mock server, but the
    // pinned jwks_uri targets the cloud-metadata service. The mock serves a JWKS so that ANY
    // fetch would succeed; the guard must reject BEFORE fetching, on origin mismatch alone.
    let (_server, issuer) = serve(jwks_for(TEST_RSA_N)).await;
    let mut config = config_for(&issuer, "aud-1");
    config.jwks_uri = Some("http://169.254.169.254/latest/meta-data/".to_owned());
    let err = JwtValidator::new(&config)
        .await
        .expect_err("a foreign-origin jwks_uri is refused");
    assert!(
        matches!(err, AuthError::JwksRefresh(_)),
        "config-override SSRF must be refused, got {err:?}"
    );
}

#[tokio::test]
async fn discovery_jwks_uri_on_a_foreign_origin_is_refused() {
    // SSRF guard, discovery-doc path: the document asserts the correct issuer (passing the
    // byte-for-byte issuer check) but points jwks_uri at an internal host. The origin guard
    // must reject it even though the issuer field is legitimate.
    let server = MockServer::start().await;
    let issuer = server.uri();
    // jwks_uri is a DIFFERENT host than the issuer's loopback origin.
    let discovery = json!({ "issuer": issuer, "jwks_uri": "http://127.0.0.1:3918/internal/jwks" });
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(discovery))
        .mount(&server)
        .await;
    let config = config_for(&issuer, "aud-1");
    let err = JwtValidator::new(&config)
        .await
        .expect_err("a foreign-origin discovery jwks_uri is refused");
    assert!(
        matches!(err, AuthError::Discovery(_)),
        "discovery-doc SSRF must be refused, got {err:?}"
    );
}

#[tokio::test]
async fn discovery_does_not_follow_a_redirect() {
    // The client is built with redirect Policy::none(), so a 302 from the discovery endpoint
    // is surfaced as a non-success status (a Discovery error), never followed to the Location.
    let server = MockServer::start().await;
    let issuer = server.uri();
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", "http://169.254.169.254/latest/meta-data/"),
        )
        .mount(&server)
        .await;
    let config = config_for(&issuer, "aud-1");
    let err = JwtValidator::new(&config)
        .await
        .expect_err("a redirected discovery fails closed");
    assert!(matches!(err, AuthError::Discovery(_)), "got {err:?}");
}

#[tokio::test]
async fn an_oversized_jwks_body_is_a_refresh_error() {
    // A JWKS body past the 1 MiB cap must fail as a refresh error rather than being buffered
    // whole into memory. We serve ~2 MiB of valid-JSON-prefix padding.
    let server = MockServer::start().await;
    let issuer = server.uri();
    let jwks_uri = format!("{issuer}/jwks.json");
    let discovery = json!({ "issuer": issuer, "jwks_uri": jwks_uri });
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(discovery))
        .mount(&server)
        .await;
    let oversized = format!("{{\"keys\":[{}]}}", " ".repeat(2 * 1024 * 1024));
    Mock::given(method("GET"))
        .and(path("/jwks.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(oversized))
        .mount(&server)
        .await;
    let config = config_for(&issuer, "aud-1");
    let err = JwtValidator::new(&config)
        .await
        .expect_err("an oversized JWKS body is rejected");
    assert!(matches!(err, AuthError::JwksRefresh(_)), "got {err:?}");
}

#[tokio::test]
async fn a_distinct_kid_flood_triggers_at_most_one_refetch_in_the_cooldown() {
    // Cross-call rate limit: after construction, a burst of tokens each carrying a DISTINCT
    // unknown kid must drive at most ONE JWKS refetch within the cooldown window, not one per
    // request. We bound the JWKS endpoint to exactly two hits (construction + one refetch);
    // wiremock fails the test if the flood drives a third.
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
        .respond_with(ResponseTemplate::new(200).set_body_json(jwks_for(TEST_RSA_N)))
        .expect(2)
        .mount(&server)
        .await;

    let audience = "aud-1";
    let config = config_for(&issuer, audience);
    let validator = JwtValidator::new(&config).await.expect("validator builds");

    // Twenty tokens, each with a unique kid the JWKS never contains. The first consumes the
    // single refetch slot; the rest must be answered from the cache with no network call.
    for i in 0..20 {
        let kid = format!("flood-kid-{i}");
        let token = mint_token(&kid, &standard_claims(&issuer, audience));
        let err = validator
            .validate(&token)
            .await
            .expect_err("an unknown kid is rejected");
        assert!(matches!(err, AuthError::NoMatchingKey(_)), "got {err:?}");
    }
    // `.expect(2)` is verified on drop: construction + exactly one refetch, never twenty-one.
    drop(server);
}
