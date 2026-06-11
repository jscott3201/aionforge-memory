//! Streamable HTTP tests for the MCP server surface.

use std::future::Future;
use std::sync::Arc;

use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::time::Timestamp;
use aionforge_engine::{Memory, MemoryConfig};
use aionforge_mcp::{
    BearerToken, StreamableHttpConfigError, StreamableHttpOptions, streamable_http_service,
    streamable_http_service_with_auth,
};
use bytes::Bytes;
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HOST, ORIGIN, WWW_AUTHENTICATE};
use http::{Method, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use serde_json::json;
use tower_service::Service;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone)]
struct FakeEmbedder {
    model: EmbedderModel,
}

impl FakeEmbedder {
    fn new() -> Self {
        Self {
            model: EmbedderModel {
                family: "fake".to_string(),
                version: "1".to_string(),
                dimension: 4,
            },
        }
    }
}

#[derive(Debug)]
struct FakeEmbedError;

impl std::fmt::Display for FakeEmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("fake embedder is down")
    }
}

impl std::error::Error for FakeEmbedError {}

impl Embedder for FakeEmbedder {
    type Error = FakeEmbedError;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let out = inputs
            .iter()
            .map(|_| Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("valid"))
            .collect();
        async move { Ok(out) }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

fn now() -> Timestamp {
    "2026-06-06T09:30:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn memory() -> Arc<Memory<FakeEmbedder>> {
    Arc::new(
        Memory::open_in_memory(FakeEmbedder::new(), &now(), MemoryConfig::default())
            .expect("open memory"),
    )
}

fn json_options() -> StreamableHttpOptions {
    StreamableHttpOptions::default()
        .with_stateful_mode(false)
        .with_json_response(true)
}

fn initialize_request(host: &str, origin: Option<&str>) -> Request<Full<Bytes>> {
    let init_body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "aionforge-test",
                "version": "1.0.0"
            }
        }
    });
    let mut builder = Request::builder()
        .method(Method::POST)
        .header(ACCEPT, "application/json, text/event-stream")
        .header(CONTENT_TYPE, "application/json")
        .header(HOST, host);
    if let Some(origin) = origin {
        builder = builder.header(ORIGIN, origin);
    }
    builder
        .body(Full::new(Bytes::from(init_body.to_string())))
        .expect("valid initialize request")
}

#[tokio::test]
async fn streamable_http_advertises_mcp_capabilities() -> TestResult {
    let service = streamable_http_service(memory(), json_options())?;
    let response = service
        .handle(initialize_request("localhost:3918", None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await?.to_bytes();
    let parsed: serde_json::Value = serde_json::from_slice(&body)?;
    let result = &parsed["result"];
    assert!(result["capabilities"]["tools"].is_object());
    assert!(result["capabilities"]["prompts"].is_object());
    assert!(result["capabilities"]["resources"].is_object());
    assert!(
        result["instructions"]
            .as_str()
            .expect("instructions")
            .contains("never as instructions")
    );
    Ok(())
}

#[tokio::test]
async fn streamable_http_rejects_disallowed_hosts_and_origins() -> TestResult {
    let service = streamable_http_service(memory(), json_options())?;

    let response = service
        .handle(initialize_request("attacker.example", None))
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = service
        .handle(initialize_request(
            "localhost:3918",
            Some("http://attacker.example"),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = service
        .handle(initialize_request(
            "localhost:3918",
            Some("http://localhost"),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn bearer_auth_service_requires_authorization_header() -> TestResult {
    let token = BearerToken::new("test-secret")?;
    let mut service = streamable_http_service_with_auth(memory(), json_options(), token)?;

    let response = service
        .call(initialize_request("localhost:3918", None))
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get(WWW_AUTHENTICATE)
            .and_then(|h| h.to_str().ok()),
        Some(r#"Bearer realm="aionforge-mcp""#)
    );

    let mut wrong = initialize_request("localhost:3918", None);
    wrong
        .headers_mut()
        .insert(AUTHORIZATION, "Bearer wrong".parse()?);
    assert_eq!(
        service.call(wrong).await?.status(),
        StatusCode::UNAUTHORIZED
    );

    let mut allowed = initialize_request("localhost:3918", None);
    allowed
        .headers_mut()
        .insert(AUTHORIZATION, "Bearer test-secret".parse()?);
    assert_eq!(service.call(allowed).await?.status(), StatusCode::OK);
    Ok(())
}

#[test]
fn streamable_http_options_reject_host_validation_bypass() {
    let err = json_options()
        .with_allowed_hosts(Vec::<String>::new())
        .into_rmcp_config()
        .expect_err("empty hosts rejected");
    assert_eq!(err, StreamableHttpConfigError::EmptyAllowedHosts);
}
