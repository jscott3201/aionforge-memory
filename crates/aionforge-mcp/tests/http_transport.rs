//! Streamable HTTP tests for the MCP server surface.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::time::Timestamp;
use aionforge_engine::{Memory, MemoryConfig};
use aionforge_mcp::{
    AuthPosture, DEFAULT_MAX_REQUEST_BODY_BYTES, OAuthProtectedResourceMetadata,
    STREAMABLE_HTTP_ENDPOINT, StreamableHttpConfigError, StreamableHttpOptions,
    oauth_protected_resource_well_known_path, streamable_http_service,
};
use bytes::Bytes;
use http::header::{ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, HOST, ORIGIN};
use http::{Method, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use serde_json::json;

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

/// STATEFUL JSON options — the production `serve http` posture (sessions on). Unlike
/// [`json_options`], a session-less `initialize` here traverses rmcp's
/// `session_manager.create_session()` -> `create_local_session`, which emits
/// `tracing::info!("create new session")`. That `info!` emit-on-a-worker-thread is exactly
/// the event that deadlocked the #252 `initialize` hang, so the regression test must use this
/// posture (the prior smoke test used stateless options and never reached the emit site).
fn stateful_options() -> StreamableHttpOptions {
    StreamableHttpOptions::default()
        .with_stateful_mode(true)
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

fn tool_call_request(host: &str, name: &str, arguments: serde_json::Value) -> Request<Full<Bytes>> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": name,
            "arguments": arguments
        }
    });
    Request::builder()
        .method(Method::POST)
        .header(ACCEPT, "application/json, text/event-stream")
        .header(CONTENT_TYPE, "application/json")
        .header(HOST, host)
        .header("MCP-Protocol-Version", "2025-03-26")
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("valid tool call request")
}

#[tokio::test]
async fn streamable_http_advertises_mcp_capabilities() -> TestResult {
    let service = streamable_http_service(memory(), json_options(), AuthPosture::disabled())?;
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

/// Regression for the #252 `serve http` `initialize` HANG (steward task 019ec919-5bc2).
///
/// The bug: installing a synchronous stderr `tracing` subscriber, while `main` held the
/// process-global stdio lock across the parked `serve`, deadlocked the first STATEFUL
/// `initialize`. On a fresh (session-less) `initialize`, rmcp calls `create_local_session`,
/// which emits `tracing::info!("create new session")` on a tokio WORKER thread; that worker's
/// `Stderr::write_all` blocked forever on the std reentrant lock the parked main thread owned.
/// The request never returned and (the blocking write *being* the log emission) no log line
/// appeared. The pre-existing smoke test missed this on three axes, all closed here:
///   1. it installed NO `tracing` subscriber (no dispatch-to-stderr writer);
///   2. it used `with_stateful_mode(false)`, so it never reached `create_local_session`'s
///      `info!` (the stateless path only emits `trace!`, below the `info` filter);
///   3. it had no timeout, so a hang would wedge rather than fail.
///
/// This test assembles the dispatch-to-stderr half (a real `info`-level stderr fmt subscriber,
/// mirroring `observability::init`), drives a STATEFUL session-less `initialize` so the
/// `info!("create new session")` event actually fires, and bounds it with a timeout so a
/// regression surfaces as a failed assertion, never a wedged CI run. With the fix (main no
/// longer holds the stdio lock across serve), the worker's stderr write completes and the
/// handshake returns a JSON-RPC result well within the bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stateful_initialize_does_not_hang_with_stderr_subscriber() -> TestResult {
    use tracing_subscriber::EnvFilter;

    // Install a real stderr-writing fmt subscriber at `info` (the default level), mirroring
    // `observability::init`, so the `info!("create new session")` event is actually dispatched
    // to a stderr writer rather than the no-op global. `try_init` is a harmless no-op if some
    // other test in this binary already installed a global subscriber — the assertion below is
    // what matters, and it holds for any installed-or-not subscriber state.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::new("info"))
        .try_init();

    let service = streamable_http_service(memory(), stateful_options(), AuthPosture::disabled())?;

    // A session-less `initialize` against a STATEFUL service creates a new session, which is the
    // `info!("create new session")` emit site. Bound it: pre-fix this never returned.
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        service.handle(initialize_request("localhost:3918", None)),
    )
    .await
    .expect("stateful initialize must not hang (the #252 deadlock would time out here)");

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await?.to_bytes();
    // A STATEFUL `initialize` always replies as an SSE stream (rmcp returns `text/event-stream`
    // for the session-creating handshake regardless of `json_response`, with the session id in
    // the `Mcp-Session-Id` header), so the JSON-RPC envelope rides an SSE `data:` line. Parse it
    // out — this mirrors the real `serve http` production posture the Claude client drives.
    let parsed = parse_sse_json_rpc(std::str::from_utf8(&body)?)
        .unwrap_or_else(|| panic!("initialize SSE body must carry a JSON-RPC data line: {body:?}"));
    assert_eq!(parsed["jsonrpc"], "2.0", "{parsed}");
    assert!(
        parsed["result"]["serverInfo"].is_object(),
        "initialize must return a serverInfo result: {parsed}"
    );
    assert!(
        parsed["result"]["capabilities"]["tools"].is_object(),
        "{parsed}"
    );
    Ok(())
}

/// Extract the first JSON-RPC payload from an SSE body's `data:` line(s).
///
/// rmcp's stateful streamable-HTTP transport frames the `initialize` response with a leading
/// priming event (`data:` empty, plus `id:`/`retry:` lines) followed by the real
/// `data: {json}` line. Scan every `data:` line and return the first whose value parses as
/// JSON, skipping the empty priming line.
fn parse_sse_json_rpc(body: &str) -> Option<serde_json::Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .find_map(|json| serde_json::from_str(json).ok())
}

#[tokio::test]
async fn streamable_http_rejects_disallowed_hosts_and_origins() -> TestResult {
    let service = streamable_http_service(memory(), json_options(), AuthPosture::disabled())?;

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
async fn streamable_http_rejects_oversized_request_bodies() -> TestResult {
    let service = streamable_http_service(
        memory(),
        json_options().with_max_request_body_bytes("{}".len()),
        AuthPosture::disabled(),
    )?;

    let response = service
        .handle(initialize_request("localhost:3918", None))
        .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = response.into_body().collect().await?.to_bytes();
    assert!(
        std::str::from_utf8(&body)?.contains("request body exceeds"),
        "{body:?}"
    );
    Ok(())
}

#[tokio::test]
async fn streamable_http_rejects_content_length_over_limit_before_reading() -> TestResult {
    let service = streamable_http_service(
        memory(),
        json_options().with_max_request_body_bytes(DEFAULT_MAX_REQUEST_BODY_BYTES),
        AuthPosture::disabled(),
    )?;
    let mut request = initialize_request("localhost:3918", None);
    request.headers_mut().insert(
        CONTENT_LENGTH,
        (DEFAULT_MAX_REQUEST_BODY_BYTES + 1).to_string().parse()?,
    );

    let response = service.handle(request).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    Ok(())
}

#[tokio::test]
async fn http_tool_calls_do_not_require_authorization_header() -> TestResult {
    let alice = "018f0cc0-40f3-7cc4-b8b4-9ca41f88d012";
    let service = streamable_http_service(memory(), json_options(), AuthPosture::disabled())?;

    let request = tool_call_request(
        "localhost:3918",
        "capture",
        json!({
            "content": "the local HTTP baseline has no transport auth secret",
            "agent_id": alice,
        }),
    );
    let response = service.handle(request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await?.to_bytes();
    let parsed: serde_json::Value = serde_json::from_slice(&body)?;
    let text = parsed["result"]["content"][0]["text"]
        .as_str()
        .expect("tool text");
    assert!(text.starts_with("[capture] "), "tool response: {parsed}");
    Ok(())
}

#[test]
fn oauth_protected_resource_metadata_uses_rfc9728_shape() {
    assert_eq!(
        oauth_protected_resource_well_known_path(STREAMABLE_HTTP_ENDPOINT),
        "/.well-known/oauth-protected-resource/mcp"
    );

    let metadata = OAuthProtectedResourceMetadata::new(
        "https://memory.example.com/mcp",
        ["https://auth.example.com"],
    )
    .with_scopes(["aionforge:read", "aionforge:write"])
    .with_resource_documentation("https://docs.example.com/aionforge/mcp")
    .with_resource_policy_uri("https://docs.example.com/aionforge/policy");
    let value: serde_json::Value = serde_json::from_str(&metadata.to_json()).expect("json");
    assert_eq!(value["resource"], "https://memory.example.com/mcp");
    assert_eq!(
        value["authorization_servers"],
        serde_json::json!(["https://auth.example.com"])
    );
    assert_eq!(
        value["scopes_supported"],
        serde_json::json!(["aionforge:read", "aionforge:write"])
    );
    assert_eq!(
        value["bearer_methods_supported"],
        serde_json::json!(["header"])
    );
    assert_eq!(value["resource_name"], "Aionforge Memory MCP");
    assert_eq!(
        value["resource_documentation"],
        "https://docs.example.com/aionforge/mcp"
    );
    assert_eq!(
        value["resource_policy_uri"],
        "https://docs.example.com/aionforge/policy"
    );
}

#[test]
fn streamable_http_options_reject_host_validation_bypass() {
    let err = json_options()
        .with_allowed_hosts(Vec::<String>::new())
        .into_rmcp_config()
        .expect_err("empty hosts rejected");
    assert_eq!(err, StreamableHttpConfigError::EmptyAllowedHosts);
}

#[test]
fn streamable_http_options_reject_zero_body_limit() {
    let err = json_options()
        .with_max_request_body_bytes(0)
        .into_rmcp_config()
        .expect_err("zero body limit rejected");
    assert_eq!(err, StreamableHttpConfigError::ZeroMaxRequestBodyBytes);
}
