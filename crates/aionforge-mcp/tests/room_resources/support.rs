use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use aionforge_domain::ids::Id;
use aionforge_engine::{Memory, Principal};
use aionforge_mcp::{
    AionforgeStreamableHttpService, AuthPosture, MessageWaitBounds, RoomSubscribeBounds,
    StreamableHttpOptions, TokenClass, ValidatedPrincipal, WritePosture, streamable_http_service,
    streamable_http_service_with_consolidation_and_message_wait,
};
use bytes::Bytes;
use http::header::{ACCEPT, CONTENT_TYPE, HOST};
use http::{Method, Request, StatusCode};
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use serde_json::{Value, json};

use crate::common::FakeEmbedder;

pub type TestError = Box<dyn std::error::Error + Send + Sync>;
pub type TestResult<T = ()> = Result<T, TestError>;
pub type Service = AionforgeStreamableHttpService<FakeEmbedder>;
pub type EventBody = BoxBody<Bytes, Infallible>;

pub const HOST_VALUE: &str = "localhost:3918";
const PROTOCOL_VERSION: &str = "2025-03-26";

#[derive(Clone)]
pub struct Session {
    pub id: String,
    pub principal: ValidatedPrincipal,
}

pub fn writer(agent: Id, teams: &[&str]) -> ValidatedPrincipal {
    ValidatedPrincipal::new(
        Principal::new(agent, teams.iter().map(ToString::to_string).collect()),
        WritePosture::Writer,
        TokenClass::Machine,
    )
}

pub fn stateful_auth_service(
    memory: Arc<Memory<FakeEmbedder>>,
    room_bounds: RoomSubscribeBounds,
) -> Service {
    streamable_http_service_with_consolidation_and_message_wait(
        memory,
        StreamableHttpOptions::default()
            .with_stateful_mode(true)
            .with_json_response(true),
        AuthPosture::enabled(vec!["https://issuer.example/".to_string()]),
        false,
        MessageWaitBounds::default(),
        room_bounds,
    )
    .expect("stateful auth service builds")
}

pub fn stateless_service(memory: Arc<Memory<FakeEmbedder>>) -> Service {
    streamable_http_service(
        memory,
        StreamableHttpOptions::default()
            .with_stateful_mode(false)
            .with_json_response(true),
        AuthPosture::disabled(),
    )
    .expect("stateless service builds")
}

pub async fn open_session(service: &Service, principal: ValidatedPrincipal) -> TestResult<Session> {
    let response = service
        .handle(post_request(
            1,
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "room-resource-test", "version": "1.0" },
            }),
            None,
            Some(&principal),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let session_id = response
        .headers()
        .get("Mcp-Session-Id")
        .expect("stateful initialize assigns a session")
        .to_str()?
        .to_owned();
    let response = finite_sse_response(response, 1).await?;
    assert!(
        response["result"].is_object(),
        "initialize succeeds: {response}"
    );
    Ok(Session {
        id: session_id,
        principal,
    })
}

pub async fn rpc(
    service: &Service,
    session: &Session,
    id: u64,
    method: &str,
    params: Value,
) -> TestResult<Value> {
    let response = service
        .handle(post_request(
            id,
            method,
            params,
            Some(&session.id),
            Some(&session.principal),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK, "{method} HTTP response");
    finite_sse_response(response, id).await
}

pub async fn stateless_rpc(
    service: &Service,
    id: u64,
    method: &str,
    params: Value,
) -> TestResult<Value> {
    let response = service
        .handle(post_request(id, method, params, None, None))
        .await;
    assert_eq!(response.status(), StatusCode::OK, "{method} HTTP response");
    let body = response.into_body().collect().await?.to_bytes();
    Ok(serde_json::from_slice(&body)?)
}

pub async fn subscribe(
    service: &Service,
    session: &Session,
    id: u64,
    uri: &str,
) -> TestResult<Value> {
    rpc(
        service,
        session,
        id,
        "resources/subscribe",
        json!({ "uri": uri }),
    )
    .await
}

pub async fn unsubscribe(
    service: &Service,
    session: &Session,
    id: u64,
    uri: &str,
) -> TestResult<Value> {
    rpc(
        service,
        session,
        id,
        "resources/unsubscribe",
        json!({ "uri": uri }),
    )
    .await
}

pub async fn read_resource(
    service: &Service,
    session: &Session,
    id: u64,
    uri: &str,
) -> TestResult<Value> {
    rpc(
        service,
        session,
        id,
        "resources/read",
        json!({ "uri": uri }),
    )
    .await
}

pub async fn send_room_message(
    service: &Service,
    session: &Session,
    id: u64,
    recipient: &str,
    room: Id,
    body: &str,
) -> TestResult<Value> {
    rpc(
        service,
        session,
        id,
        "tools/call",
        json!({
            "name": "message_send",
            "arguments": {
                "to": recipient,
                "body": body,
                "room_id": room.to_string(),
            },
        }),
    )
    .await
}

pub async fn message_poll(
    service: &Service,
    session: &Session,
    id: u64,
    room: Id,
) -> TestResult<Value> {
    rpc(
        service,
        session,
        id,
        "tools/call",
        json!({
            "name": "message_poll",
            "arguments": { "room_id": room.to_string() },
        }),
    )
    .await
}

pub async fn open_events(service: &Service, session: &Session) -> TestResult<EventBody> {
    let request = Request::builder()
        .method(Method::GET)
        .header(ACCEPT, "text/event-stream")
        .header(HOST, HOST_VALUE)
        .header("MCP-Protocol-Version", PROTOCOL_VERSION)
        .header("Mcp-Session-Id", &session.id)
        .body(Full::new(Bytes::new()))?;
    let response = service.handle(request).await;
    assert_eq!(response.status(), StatusCode::OK);
    Ok(response.into_body())
}

pub async fn close_session(service: &Service, session: &Session) -> TestResult {
    let request = Request::builder()
        .method(Method::DELETE)
        .header(HOST, HOST_VALUE)
        .header("MCP-Protocol-Version", PROTOCOL_VERSION)
        .header("Mcp-Session-Id", &session.id)
        .body(Full::new(Bytes::new()))?;
    let response = service.handle(request).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let _ = response.into_body().collect().await?;
    Ok(())
}

pub async fn resource_update_for(
    body: &mut EventBody,
    uri: &str,
    within: Duration,
) -> TestResult<Option<Value>> {
    match tokio::time::timeout(within, wait_for_resource_update(body, uri)).await {
        Ok(result) => result,
        Err(_) => Ok(None),
    }
}

pub fn resource_text(response: &Value) -> TestResult<String> {
    response["result"]["contents"]
        .as_array()
        .and_then(|contents| contents.first())
        .and_then(|content| content["text"].as_str())
        .map(ToString::to_string)
        .ok_or_else(|| failure(format!("expected text resource result: {response}")))
}

pub fn tool_text(response: &Value) -> TestResult<String> {
    response["result"]["content"]
        .as_array()
        .and_then(|content| content.first())
        .and_then(|content| content["text"].as_str())
        .map(ToString::to_string)
        .ok_or_else(|| failure(format!("expected tool text result: {response}")))
}

pub fn assert_resource_not_found(response: &Value) {
    assert_eq!(response["error"]["code"], -32_002, "{response}");
}

fn post_request(
    id: u64,
    method: &str,
    params: Value,
    session_id: Option<&str>,
    principal: Option<&ValidatedPrincipal>,
) -> Request<Full<Bytes>> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let mut builder = Request::builder()
        .method(Method::POST)
        .header(ACCEPT, "application/json, text/event-stream")
        .header(CONTENT_TYPE, "application/json")
        .header(HOST, HOST_VALUE)
        .header("MCP-Protocol-Version", PROTOCOL_VERSION);
    if let Some(session_id) = session_id {
        builder = builder.header("Mcp-Session-Id", session_id);
    }
    let mut request = builder
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("valid JSON-RPC request");
    if let Some(principal) = principal {
        request.extensions_mut().insert(principal.clone());
    }
    request
}

async fn finite_sse_response(
    response: http::Response<EventBody>,
    expected_id: u64,
) -> TestResult<Value> {
    let body = tokio::time::timeout(Duration::from_secs(2), response.into_body().collect())
        .await
        .map_err(|_| failure("stateful request response timed out"))??
        .to_bytes();
    sse_messages(&body)
        .into_iter()
        .find(|message| message["id"] == expected_id)
        .ok_or_else(|| failure(format!("missing JSON-RPC response id {expected_id}")))
}

async fn wait_for_resource_update(body: &mut EventBody, uri: &str) -> TestResult<Option<Value>> {
    loop {
        let Some(frame) = body.frame().await else {
            return Ok(None);
        };
        let frame = frame.expect("streamable HTTP body is infallible");
        let Ok(data) = frame.into_data() else {
            continue;
        };
        for message in sse_messages(&data) {
            if message["method"] == "notifications/resources/updated"
                && message["params"]["uri"] == uri
            {
                return Ok(Some(message));
            }
        }
    }
}

fn sse_messages(body: &[u8]) -> Vec<Value> {
    std::str::from_utf8(body)
        .expect("SSE payload is UTF-8")
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter_map(|json| serde_json::from_str(json).ok())
        .collect()
}

fn failure(message: impl Into<String>) -> TestError {
    std::io::Error::other(message.into()).into()
}
