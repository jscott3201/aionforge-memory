//! Acceptance coverage for opt-in progress heartbeats on bounded message waits.

mod common;

use std::time::Duration;

use aionforge_domain::ids::Id;
use aionforge_mcp::{AionforgeMcp, MessageWaitBounds};
use common::{FakeEmbedder, memory};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest, Meta, NumberOrString,
    ProgressNotificationParam, ProgressToken, ServerResult,
};
use rmcp::service::{NotificationContext, PeerRequestOptions, RunningService};
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

type TestError = Box<dyn std::error::Error + Send + Sync>;
type TestResult<T = ()> = Result<T, TestError>;
type RecordingService = RunningService<RoleClient, RecordingClient>;

#[derive(Clone)]
struct RecordingClient {
    progress: UnboundedSender<ProgressNotificationParam>,
}

impl ClientHandler for RecordingClient {
    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        let _ = self.progress.send(params);
    }
}

struct RecordingHarness {
    client: RecordingService,
    progress: UnboundedReceiver<ProgressNotificationParam>,
    server: tokio::task::JoinHandle<Result<(), TestError>>,
}

impl RecordingHarness {
    async fn connect(server: AionforgeMcp<FakeEmbedder>) -> TestResult<Self> {
        let (server_transport, client_transport) = tokio::io::duplex(32 * 1024);
        let server = tokio::spawn(async move {
            let service = server.serve(server_transport).await?;
            service.waiting().await?;
            Ok(())
        });
        let (sender, progress) = unbounded_channel();
        let client = RecordingClient { progress: sender }
            .serve(client_transport)
            .await?;
        Ok(Self {
            client,
            progress,
            server,
        })
    }

    async fn shutdown(self) -> TestResult {
        self.client.cancel().await?;
        self.server.await??;
        Ok(())
    }
}

fn bounds(wait_seconds: u64) -> MessageWaitBounds {
    MessageWaitBounds {
        default_seconds: wait_seconds,
        max_seconds: wait_seconds,
        max_concurrent: 4,
        max_recipients: 256,
        heartbeat_seconds: 1,
    }
}

fn wait_params(reader: Id, timeout_seconds: u64) -> CallToolRequestParams {
    CallToolRequestParams::new("message_wait").with_arguments(object_args(json!({
        "viewer": format!("agent:{reader}"),
        "timeout_seconds": timeout_seconds,
    })))
}

async fn call_wait_with_token(
    client: &RecordingService,
    reader: Id,
    timeout_seconds: u64,
    token: ProgressToken,
) -> Result<CallToolResult, rmcp::ServiceError> {
    let request =
        ClientRequest::CallToolRequest(CallToolRequest::new(wait_params(reader, timeout_seconds)));
    let mut options = PeerRequestOptions::no_options();
    options.meta = Some(Meta::with_progress_token(token));
    let response = client
        .send_cancellable_request(request, options)
        .await?
        .await_response()
        .await?;
    match response {
        ServerResult::CallToolResult(result) => Ok(result),
        _ => Err(rmcp::ServiceError::UnexpectedResponse),
    }
}

async fn call_send(
    client: &RecordingService,
    sender: Id,
    recipient: &str,
    body: &str,
) -> Result<CallToolResult, rmcp::ServiceError> {
    client
        .call_tool(
            CallToolRequestParams::new("message_send").with_arguments(object_args(json!({
                "viewer": format!("agent:{sender}"),
                "to": recipient,
                "body": body,
            }))),
        )
        .await
}

fn assert_heartbeat(frame: &ProgressNotificationParam, token: &ProgressToken, total_seconds: f64) {
    assert_eq!(&frame.progress_token, token);
    assert_eq!(frame.total, Some(total_seconds));
    assert_eq!(frame.message.as_deref(), Some("still waiting; 0 new"));
    assert!(frame.progress >= 0.0);
    assert!(frame.progress < total_seconds);
}

#[tokio::test]
async fn heartbeats_arrive_while_parked_and_progress_is_monotonic() -> TestResult {
    let reader = Id::generate();
    let token = ProgressToken(NumberOrString::Number(1));
    let mut harness = RecordingHarness::connect(AionforgeMcp::new_with_message_wait_bounds(
        memory(),
        bounds(3),
    ))
    .await?;
    let result = {
        let wait = call_wait_with_token(&harness.client, reader, 3, token.clone());
        tokio::pin!(wait);
        let mut frames = Vec::new();

        tokio::time::timeout(Duration::from_secs(4), async {
            while frames.len() < 2 {
                tokio::select! {
                    result = &mut wait => panic!("wait ended before two heartbeats: {result:?}"),
                    frame = harness.progress.recv() => frames.push(frame.expect("progress frame")),
                }
            }
        })
        .await?;

        for frame in &frames {
            assert_heartbeat(frame, &token, 3.0);
        }
        assert!(
            frames
                .windows(2)
                .all(|frames| frames[0].progress <= frames[1].progress),
            "progress must be non-decreasing: {frames:?}",
        );
        tokio::time::timeout(Duration::from_secs(2), &mut wait).await??
    };
    assert_eq!(structured(&result)["timed_out"], true);
    assert_eq!(structured(&result)["messages"], json!([]));
    harness.shutdown().await
}

#[tokio::test]
async fn heartbeat_path_rearms_and_stops_after_message_wake() -> TestResult {
    let sender = Id::generate();
    let reader = Id::generate();
    let token = ProgressToken(NumberOrString::Number(2));
    let mut harness = RecordingHarness::connect(AionforgeMcp::new_with_message_wait_bounds(
        memory(),
        bounds(5),
    ))
    .await?;
    let result = {
        let wait = call_wait_with_token(&harness.client, reader, 5, token.clone());
        tokio::pin!(wait);
        let first = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                result = &mut wait => panic!("wait ended before its first heartbeat: {result:?}"),
                frame = harness.progress.recv() => frame.expect("progress frame"),
            }
        })
        .await?;
        assert_heartbeat(&first, &token, 5.0);

        call_send(
            &harness.client,
            sender,
            &format!("agent:{reader}"),
            "heartbeat wake sentinel",
        )
        .await?;
        tokio::time::timeout(Duration::from_secs(3), &mut wait).await??
    };
    assert_eq!(structured(&result)["timed_out"], false);
    assert_eq!(structured(&result)["count"], 1);
    assert!(first_text(&result).contains("heartbeat wake sentinel"));

    while harness.progress.try_recv().is_ok() {}
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert!(harness.progress.try_recv().is_err());
    harness.shutdown().await
}

#[tokio::test]
async fn heartbeat_stops_after_timeout() -> TestResult {
    let reader = Id::generate();
    let token = ProgressToken(NumberOrString::Number(3));
    let mut harness = RecordingHarness::connect(AionforgeMcp::new_with_message_wait_bounds(
        memory(),
        bounds(3),
    ))
    .await?;
    let result = {
        let wait = call_wait_with_token(&harness.client, reader, 3, token.clone());
        tokio::pin!(wait);
        let first = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                result = &mut wait => panic!("wait ended before its first heartbeat: {result:?}"),
                frame = harness.progress.recv() => frame.expect("progress frame"),
            }
        })
        .await?;
        assert_heartbeat(&first, &token, 3.0);

        tokio::time::timeout(Duration::from_secs(3), &mut wait).await??
    };
    assert_eq!(structured(&result)["timed_out"], true);
    while harness.progress.try_recv().is_ok() {}
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert!(harness.progress.try_recv().is_err());
    harness.shutdown().await
}

#[tokio::test]
async fn cancelling_a_heartbeat_client_releases_admission_for_later_delivery() -> TestResult {
    let sender = Id::generate();
    let reader = Id::generate();
    let mut bounds = bounds(5);
    bounds.max_concurrent = 1;
    let template = AionforgeMcp::new_with_message_wait_bounds(memory(), bounds);
    let mut cancelled = RecordingHarness::connect(template.clone()).await?;

    {
        let wait = call_wait_with_token(
            &cancelled.client,
            reader,
            5,
            ProgressToken(NumberOrString::Number(4)),
        );
        tokio::pin!(wait);
        let first = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                result = &mut wait => panic!("wait ended before its first heartbeat: {result:?}"),
                frame = cancelled.progress.recv() => frame.expect("progress frame"),
            }
        })
        .await?;
        assert_heartbeat(&first, &ProgressToken(NumberOrString::Number(4)), 5.0);
    }
    cancelled.shutdown().await?;

    let waiter = RecordingHarness::connect(template.clone()).await?;
    let courier = RecordingHarness::connect(template).await?;
    let (waited, sent) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            call_wait_with_token(
                &waiter.client,
                reader,
                5,
                ProgressToken(NumberOrString::Number(5)),
            ),
            async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                call_send(
                    &courier.client,
                    sender,
                    &format!("agent:{reader}"),
                    "delivery after heartbeat client cancellation",
                )
                .await
            },
        )
    })
    .await?;
    sent?;
    let waited = waited?;
    assert_eq!(structured(&waited)["timed_out"], false);
    assert!(first_text(&waited).contains("delivery after heartbeat client cancellation"));
    waiter.shutdown().await?;
    courier.shutdown().await
}

#[tokio::test]
async fn no_progress_token_keeps_the_raw_duplex_wait_heartbeat_free() -> TestResult {
    let reader = Id::generate();
    let (server_transport, client_transport) = tokio::io::duplex(32 * 1024);
    let server = AionforgeMcp::new_with_message_wait_bounds(memory(), bounds(2));
    let server = tokio::spawn(async move {
        let service = server.serve(server_transport).await?;
        service.waiting().await?;
        Ok::<(), TestError>(())
    });
    let (read, mut write) = tokio::io::split(client_transport);
    let mut read = BufReader::new(read);

    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "heartbeat-test", "version": "1.0" },
        },
    });
    write.write_all(initialize.to_string().as_bytes()).await?;
    write.write_all(b"\n").await?;
    write.flush().await?;
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(1), read.read_line(&mut line)).await??;
    assert_eq!(serde_json::from_str::<Value>(&line)?["id"], 1);

    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {},
    });
    write.write_all(initialized.to_string().as_bytes()).await?;
    write.write_all(b"\n").await?;
    let wait = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "message_wait",
            "arguments": {
                "viewer": format!("agent:{reader}"),
                "timeout_seconds": 2,
            },
        },
    });
    write.write_all(wait.to_string().as_bytes()).await?;
    write.write_all(b"\n").await?;
    write.flush().await?;

    let response = tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            line.clear();
            let read = read.read_line(&mut line).await?;
            assert_ne!(read, 0, "server closed before message_wait responded");
            let frame: Value = serde_json::from_str(&line)?;
            assert_ne!(frame["method"], "notifications/progress", "{frame}");
            if frame["id"] == 2 {
                return Ok::<Value, TestError>(frame);
            }
        }
    })
    .await??;
    assert_eq!(response["result"]["structuredContent"]["timed_out"], true);

    drop(write);
    drop(read);
    server.await??;
    Ok(())
}

fn object_args(value: Value) -> serde_json::Map<String, Value> {
    value.as_object().expect("tool args object").clone()
}

fn first_text(result: &CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|content| content.raw.as_text())
        .map(|text| text.text.to_string())
        .unwrap_or_else(|| format!("{result:?}"))
}

fn structured(result: &CallToolResult) -> &Value {
    result
        .structured_content
        .as_ref()
        .expect("structured message_wait output")
}
