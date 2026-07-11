//! Acceptance coverage for bounded, notified message long-polling.

mod common;

use std::time::Duration;

use aionforge_domain::ids::Id;
use aionforge_domain::nodes::message::MessageReadState;
use aionforge_mcp::{
    AionforgeMcp, AuthEnabled, MessageSendToolParams, MessageWaitBounds, MessageWaitToolParams,
    message_send_tool, message_wait_tool,
};
use common::{FakeEmbedder, memory, now};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};

const OFF: AuthEnabled = AuthEnabled(false);
type TestError = Box<dyn std::error::Error + Send + Sync>;
type TestResult<T = ()> = Result<T, TestError>;
type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

struct Harness {
    client: Client,
    server: tokio::task::JoinHandle<Result<(), TestError>>,
}

impl Harness {
    async fn connect(server: AionforgeMcp<FakeEmbedder>) -> TestResult<Self> {
        let (server_transport, client_transport) = tokio::io::duplex(32 * 1024);
        let server = tokio::spawn(async move {
            let service = server.serve(server_transport).await?;
            service.waiting().await?;
            Ok(())
        });
        let client = ().serve(client_transport).await?;
        Ok(Self { client, server })
    }

    async fn shutdown(self) -> TestResult {
        self.client.cancel().await?;
        self.server.await??;
        Ok(())
    }
}

fn send_params(sender: Id, recipient: String, body: &str) -> MessageSendToolParams {
    MessageSendToolParams {
        to: recipient,
        body: body.to_string(),
        room_id: None,
        thread_id: None,
        reply_to_id: None,
        msg_kind: None,
        viewer: Some(format!("agent:{sender}")),
        principal: None,
        teams: Vec::new(),
    }
}

fn wait_params(reader: Id, timeout_seconds: u64) -> MessageWaitToolParams {
    MessageWaitToolParams {
        room_id: None,
        after: None,
        limit: None,
        unread_only: None,
        timeout_seconds: Some(timeout_seconds),
        viewer: Some(format!("agent:{reader}")),
        principal: None,
        teams: Vec::new(),
    }
}

async fn call_wait(
    client: &Client,
    reader: Id,
    teams: &[&str],
    timeout_seconds: u64,
) -> Result<CallToolResult, rmcp::ServiceError> {
    client
        .call_tool(
            CallToolRequestParams::new("message_wait").with_arguments(object_args(json!({
                "viewer": format!("agent:{reader}"),
                "teams": teams,
                "timeout_seconds": timeout_seconds,
            }))),
        )
        .await
}

async fn call_send(
    client: &Client,
    sender: Id,
    recipient: &str,
    teams: &[&str],
    body: &str,
) -> Result<CallToolResult, rmcp::ServiceError> {
    client
        .call_tool(
            CallToolRequestParams::new("message_send").with_arguments(object_args(json!({
                "viewer": format!("agent:{sender}"),
                "to": recipient,
                "teams": teams,
                "body": body,
            }))),
        )
        .await
}

#[tokio::test]
async fn returns_immediately_when_a_message_is_pending() -> TestResult {
    let memory = memory();
    let alice = Id::generate();
    let bob = Id::generate();
    let receipt = message_send_tool(
        &memory,
        send_params(alice, format!("agent:{bob}"), "already pending"),
        &now(),
        None,
        OFF,
    )?;
    let message_id = Id::parse(receipt.split_whitespace().nth(1).expect("receipt id"))?;

    let output = tokio::time::timeout(
        Duration::from_secs(1),
        message_wait_tool(&memory, wait_params(bob, 5), None, OFF),
    )
    .await??;
    assert!(output.contains("timed_out=false count=1"), "{output}");
    assert!(output.contains("already pending"), "{output}");
    assert_eq!(
        memory
            .store()
            .message_by_id(&message_id)?
            .expect("pending message")
            .read_state,
        MessageReadState::Unread,
        "message_wait never auto-acks",
    );
    Ok(())
}

#[tokio::test]
async fn timeout_returns_an_empty_wrapped_page() -> TestResult {
    let memory = memory();
    let reader = Id::generate();
    let output = tokio::time::timeout(
        Duration::from_secs(2),
        message_wait_tool(&memory, wait_params(reader, 1), None, OFF),
    )
    .await??;
    assert!(output.contains("timed_out=true count=0"), "{output}");
    assert!(output.contains("next=none"), "{output}");
    assert!(
        output.contains("<recalled-memory-context note="),
        "{output}"
    );
    assert!(output.contains("</recalled-memory-context>"), "{output}");
    Ok(())
}

#[tokio::test]
async fn over_configured_recipient_cap_serves_pending_page_without_parking() -> TestResult {
    let memory = memory();
    let sender = Id::generate();
    let reader = Id::generate();
    let teams = (0..300)
        .map(|index| format!("team-{index}"))
        .collect::<Vec<_>>();
    let target = teams.last().expect("target team").clone();
    let mut send = send_params(
        sender,
        format!("team:{target}"),
        "bounded recipient sentinel",
    );
    send.teams = vec![target];
    message_send_tool(&memory, send, &now(), None, OFF)?;

    let mut wait = wait_params(reader, 5);
    wait.teams = teams;
    let output = tokio::time::timeout(
        Duration::from_secs(1),
        message_wait_tool(&memory, wait, None, OFF),
    )
    .await??;
    assert!(output.contains("timed_out=true count=1"), "{output}");
    assert!(output.contains("bounded recipient sentinel"), "{output}");
    Ok(())
}

#[tokio::test]
async fn another_agents_dm_does_not_surface_from_a_wait() -> TestResult {
    let memory = memory();
    let sender = Id::generate();
    let alice = Id::generate();
    let bob = Id::generate();
    let template = AionforgeMcp::new(memory);
    let waiter = Harness::connect(template.clone()).await?;
    let courier = Harness::connect(template).await?;

    let wait = call_wait(&waiter.client, alice, &[], 1);
    let send = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        call_send(
            &courier.client,
            sender,
            &format!("agent:{bob}"),
            &[],
            "bob-only wake isolation sentinel",
        )
        .await
    };
    let (waited, sent) =
        tokio::time::timeout(Duration::from_secs(2), async { tokio::join!(wait, send) }).await?;
    sent?;
    let waited = waited?;
    assert_eq!(structured(&waited)["timed_out"], true);
    assert_eq!(structured(&waited)["count"], 0);
    assert!(!first_text(&waited).contains("bob-only"));
    waiter.shutdown().await?;
    courier.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn team_wait_requires_membership_and_wakes_members() -> TestResult {
    let memory = memory();
    let sender = Id::generate();
    let member = Id::generate();
    let outsider = Id::generate();
    let template = AionforgeMcp::new(memory);
    let waiter = Harness::connect(template.clone()).await?;
    let courier = Harness::connect(template.clone()).await?;
    let outsider_client = Harness::connect(template).await?;

    let wait = call_wait(&waiter.client, member, &["squad"], 5);
    let send = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        call_send(
            &courier.client,
            sender,
            "team:squad",
            &["squad"],
            "team wake sentinel",
        )
        .await
    };
    let (waited, sent) =
        tokio::time::timeout(Duration::from_secs(1), async { tokio::join!(wait, send) }).await?;
    sent?;
    let waited = waited?;
    assert_eq!(structured(&waited)["timed_out"], false);
    assert!(first_text(&waited).contains("team wake sentinel"));

    let hidden = tokio::time::timeout(
        Duration::from_secs(2),
        call_wait(&outsider_client.client, outsider, &[], 1),
    )
    .await??;
    assert_eq!(structured(&hidden)["timed_out"], true);
    assert_eq!(structured(&hidden)["count"], 0);
    waiter.shutdown().await?;
    courier.shutdown().await?;
    outsider_client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn immediate_send_cannot_be_lost_between_poll_and_await() -> TestResult {
    let memory = memory();
    let sender = Id::generate();
    let reader = Id::generate();
    let template = AionforgeMcp::new(memory);
    let waiter = Harness::connect(template.clone()).await?;
    let courier = Harness::connect(template).await?;
    let recipient = format!("agent:{reader}");

    let (waited, sent) = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(
            call_wait(&waiter.client, reader, &[], 5),
            call_send(
                &courier.client,
                sender,
                &recipient,
                &[],
                "lost wake race sentinel",
            ),
        )
    })
    .await?;
    sent?;
    let waited = waited?;
    assert_eq!(structured(&waited)["timed_out"], false);
    assert!(first_text(&waited).contains("lost wake race sentinel"));
    waiter.shutdown().await?;
    courier.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn admission_limit_sheds_a_second_wait_immediately() -> TestResult {
    let memory = memory();
    let sender = Id::generate();
    let reader = Id::generate();
    let template = AionforgeMcp::new_with_message_wait_bounds(
        memory,
        MessageWaitBounds {
            default_seconds: 5,
            max_seconds: 5,
            max_concurrent: 1,
            max_recipients: 256,
            heartbeat_seconds: 1,
        },
    );
    let first_client = Harness::connect(template.clone()).await?;
    let second_client = Harness::connect(template.clone()).await?;
    let courier = Harness::connect(template).await?;

    {
        let first = call_wait(&first_client.client, reader, &[], 5);
        tokio::pin!(first);
        tokio::select! {
            result = &mut first => panic!("first wait returned before admission test: {result:?}"),
            () = tokio::time::sleep(Duration::from_millis(75)) => {}
        }
        let second = tokio::time::timeout(
            Duration::from_millis(300),
            call_wait(&second_client.client, reader, &[], 5),
        )
        .await??;
        assert_eq!(structured(&second)["timed_out"], true);
        assert_eq!(structured(&second)["count"], 0);

        call_send(
            &courier.client,
            sender,
            &format!("agent:{reader}"),
            &[],
            "release first admission slot",
        )
        .await?;
        let first = tokio::time::timeout(Duration::from_secs(1), first).await??;
        assert_eq!(structured(&first)["timed_out"], false);
    }
    first_client.shutdown().await?;
    second_client.shutdown().await?;
    courier.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn transport_wait_preserves_private_visibility_and_structured_shape() -> TestResult {
    let memory = memory();
    let sender = Id::generate();
    let alice = Id::generate();
    let bob = Id::generate();
    let body = "bob-only wait transport message";
    message_send_tool(
        &memory,
        send_params(sender, format!("agent:{bob}"), body),
        &now(),
        None,
        OFF,
    )?;
    let harness = Harness::connect(AionforgeMcp::new(memory)).await?;

    let alice_result = tokio::time::timeout(
        Duration::from_secs(2),
        call_wait(&harness.client, alice, &[], 1),
    )
    .await??;
    assert!(!first_text(&alice_result).contains(body));
    assert_eq!(structured(&alice_result)["messages"], json!([]));

    let bob_result = tokio::time::timeout(
        Duration::from_secs(1),
        call_wait(&harness.client, bob, &[], 5),
    )
    .await??;
    assert_eq!(
        structured(&bob_result)["schema"],
        "aionforge.message_wait.v1"
    );
    assert_eq!(structured(&bob_result)["timed_out"], false);
    assert!(first_text(&bob_result).contains(body));
    assert_eq!(structured(&bob_result)["messages"][0]["body"], body);
    harness.shutdown().await?;
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
