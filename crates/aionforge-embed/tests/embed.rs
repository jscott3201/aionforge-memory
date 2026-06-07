//! Acceptance tests for the OpenAI-compatible embedding client.
//!
//! Pins the §8.1 contract: a batch embeds in input order, a wrong returned count or
//! dimension is a hard error, every vector comes back L2-normalized, the model identity
//! is recorded, an authenticated endpoint receives a bearer token, and an unreachable or
//! server-erroring endpoint surfaces as an "unavailable" error a caller can degrade on.

use std::time::Duration;

use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::EmbedderModel;
use aionforge_embed::{EmbedError, HttpEmbedder};

use secrecy::SecretString;
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn embedder(base: &str, dimension: u32, api_key: Option<&str>) -> HttpEmbedder {
    let identity = EmbedderModel {
        family: "test-model".to_owned(),
        version: String::new(),
        dimension,
    };
    HttpEmbedder::new(
        &format!("{base}/v1"),
        "test-model",
        identity,
        api_key.map(|key| SecretString::from(key.to_owned())),
        Duration::from_secs(5),
    )
    .expect("build embedder")
}

fn l2_norm(vector: &[f32]) -> f32 {
    vector.iter().map(|c| c * c).sum::<f32>().sqrt()
}

#[tokio::test]
async fn embeds_a_batch_in_input_order_and_normalizes() {
    let server = MockServer::start().await;
    // The response lists the inputs out of order (index 1 before index 0) to prove the
    // client restores input order from the index rather than trusting response order.
    let body = json!({
        "data": [
            { "index": 1, "embedding": [0.0, 3.0, 4.0] },
            { "index": 0, "embedding": [3.0, 0.0, 4.0] },
        ],
        "model": "test-model",
    });
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let embedder = embedder(&server.uri(), 3, None);
    let out = embedder
        .embed(&["first".to_owned(), "second".to_owned()])
        .await
        .expect("embed");

    assert_eq!(out.len(), 2);
    // input[0] -> [3,0,4] normalized -> dominant axis 0; input[1] -> [0,3,4] -> axis 1.
    assert!(
        out[0].as_slice()[0] > 0.5,
        "input order preserved: {:?}",
        out[0].as_slice()
    );
    assert!(
        out[1].as_slice()[1] > 0.5,
        "input order preserved: {:?}",
        out[1].as_slice()
    );
    for embedding in &out {
        assert!(
            (l2_norm(embedding.as_slice()) - 1.0).abs() < 1e-5,
            "every vector is unit length"
        );
    }
}

#[tokio::test]
async fn a_wrong_returned_count_is_a_hard_error() {
    let server = MockServer::start().await;
    let body = json!({ "data": [ { "index": 0, "embedding": [1.0, 0.0, 0.0] } ] });
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let embedder = embedder(&server.uri(), 3, None);
    let error = embedder
        .embed(&["a".to_owned(), "b".to_owned()])
        .await
        .expect_err("two inputs, one vector returned");
    assert!(
        matches!(
            error,
            EmbedError::WrongCount {
                expected: 2,
                actual: 1
            }
        ),
        "wrong count is hard: {error}"
    );
    assert!(!error.is_unavailable());
}

#[tokio::test]
async fn a_dimension_mismatch_is_a_hard_error() {
    let server = MockServer::start().await;
    let body = json!({ "data": [ { "index": 0, "embedding": [1.0, 0.0] } ] });
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    // The model is declared at dimension 3, but the endpoint returns a 2-vector.
    let embedder = embedder(&server.uri(), 3, None);
    let error = embedder
        .embed(&["a".to_owned()])
        .await
        .expect_err("dimension mismatch");
    assert!(
        matches!(
            error,
            EmbedError::DimensionMismatch {
                expected: 3,
                actual: 2
            }
        ),
        "dimension mismatch is hard: {error}"
    );
}

#[tokio::test]
async fn a_server_error_is_unavailable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let embedder = embedder(&server.uri(), 3, None);
    let error = embedder
        .embed(&["a".to_owned()])
        .await
        .expect_err("503 is unavailable");
    assert!(error.is_unavailable(), "a 5xx degrades: {error}");
}

#[tokio::test]
async fn a_client_error_is_a_hard_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let embedder = embedder(&server.uri(), 3, None);
    let error = embedder
        .embed(&["a".to_owned()])
        .await
        .expect_err("401 is a hard error");
    assert!(
        matches!(error, EmbedError::Status { status: 401 }),
        "a 4xx is surfaced, not degraded: {error}"
    );
    assert!(!error.is_unavailable());
}

#[tokio::test]
async fn an_unreachable_endpoint_is_unavailable() {
    // Port 1 on loopback refuses immediately; nothing is listening.
    let embedder = HttpEmbedder::new(
        "http://127.0.0.1:1/v1",
        "test-model",
        EmbedderModel {
            family: "test-model".to_owned(),
            version: String::new(),
            dimension: 3,
        },
        None,
        Duration::from_secs(2),
    )
    .expect("build embedder");
    let error = embedder
        .embed(&["a".to_owned()])
        .await
        .expect_err("connection refused");
    assert!(
        error.is_unavailable(),
        "an unreachable endpoint degrades: {error}"
    );
}

#[tokio::test]
async fn an_empty_batch_makes_no_request() {
    // The server mounts no responder, so any request would 404; an empty batch must
    // therefore return without touching the network.
    let server = MockServer::start().await;
    let embedder = embedder(&server.uri(), 3, None);
    let out = embedder.embed(&[]).await.expect("empty batch");
    assert!(out.is_empty());
}

#[tokio::test]
async fn a_bearer_token_is_sent_when_a_key_is_set() {
    let server = MockServer::start().await;
    // Only matches when the Authorization header carries the bearer token.
    let body = json!({ "data": [ { "index": 0, "embedding": [1.0, 0.0, 0.0] } ] });
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let embedder = embedder(&server.uri(), 3, Some("test-key"));
    let out = embedder
        .embed(&["a".to_owned()])
        .await
        .expect("authenticated embed");
    assert_eq!(out.len(), 1);
}

#[test]
fn the_model_identity_is_recorded() {
    let embedder = embedder("http://127.0.0.1:1", 1536, None);
    let model = embedder.model();
    assert_eq!(model.family, "test-model");
    assert_eq!(model.dimension, 1536);
}

#[tokio::test]
async fn a_duplicate_index_is_a_hard_error() {
    let server = MockServer::start().await;
    // Two vectors, both claiming index 0: the count is right but the indices collide.
    let body = json!({
        "data": [
            { "index": 0, "embedding": [1.0, 0.0, 0.0] },
            { "index": 0, "embedding": [0.0, 1.0, 0.0] },
        ],
    });
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let embedder = embedder(&server.uri(), 3, None);
    let error = embedder
        .embed(&["a".to_owned(), "b".to_owned()])
        .await
        .expect_err("duplicate index");
    assert!(
        matches!(error, EmbedError::Decode(_)),
        "duplicate is hard: {error}"
    );
    assert!(!error.is_unavailable());
}

#[tokio::test]
async fn an_out_of_range_index_is_a_hard_error() {
    let server = MockServer::start().await;
    // Right count (one vector for one input) but the index points outside the batch.
    let body = json!({ "data": [ { "index": 5, "embedding": [1.0, 0.0, 0.0] } ] });
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let embedder = embedder(&server.uri(), 3, None);
    let error = embedder
        .embed(&["a".to_owned()])
        .await
        .expect_err("out-of-range index");
    assert!(
        matches!(error, EmbedError::Decode(_)),
        "out of range is hard: {error}"
    );
}

#[tokio::test]
async fn a_complete_but_malformed_body_is_a_hard_decode_error() {
    let server = MockServer::start().await;
    // A full 200 response whose body is not valid JSON: a decode failure, not a transport
    // one, so it is hard rather than a degrade signal.
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{ not valid json"))
        .mount(&server)
        .await;

    let embedder = embedder(&server.uri(), 3, None);
    let error = embedder
        .embed(&["a".to_owned()])
        .await
        .expect_err("malformed body");
    assert!(
        matches!(error, EmbedError::Decode(_)),
        "malformed body is hard: {error}"
    );
    assert!(!error.is_unavailable());
}

#[test]
fn new_rejects_a_remote_plain_http_endpoint() {
    let error = HttpEmbedder::new(
        "http://remote.example.com/v1",
        "test-model",
        EmbedderModel {
            family: "test-model".to_owned(),
            version: String::new(),
            dimension: 3,
        },
        None,
        Duration::from_secs(5),
    )
    .expect_err("remote plain http is rejected at construction");
    assert!(
        matches!(error, EmbedError::Config(_)),
        "transport rule enforced in new(): {error}"
    );
}

#[test]
fn from_config_uses_the_configured_endpoint_and_identity() {
    let config = aionforge_config::EmbedderConfig {
        model: "configured-model".to_owned(),
        dimension: 768,
        ..aionforge_config::EmbedderConfig::default()
    };
    let embedder = HttpEmbedder::from_config(&config, None).expect("from_config builds");
    assert_eq!(embedder.model().family, "configured-model");
    assert_eq!(embedder.model().dimension, 768);
}
