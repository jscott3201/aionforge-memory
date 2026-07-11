//! Cancellation regression coverage for the actual `message_wait` handler future.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::time::Timestamp;
use aionforge_engine::{Memory, MemoryConfig};

use crate::AuthEnabled;
use crate::message::{
    MessageSendToolParams, MessageWaitToolParams, message_send_tool_output,
    message_wait_tool_output,
};
use crate::notify::{MessageNotifier, MessageWaitBounds};

#[derive(Clone)]
struct FakeEmbedder(EmbedderModel);

#[derive(Debug)]
struct FakeEmbedError;

impl std::fmt::Display for FakeEmbedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("fake embedder error")
    }
}

impl std::error::Error for FakeEmbedError {}

impl Embedder for FakeEmbedder {
    type Error = FakeEmbedError;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let output = inputs
            .iter()
            .map(|_| Embedding::new(vec![1.0, 0.0]).expect("valid embedding"))
            .collect();
        async move { Ok(output) }
    }

    fn model(&self) -> &EmbedderModel {
        &self.0
    }
}

fn now() -> Timestamp {
    "2026-07-10T18:00:00-04:00[America/New_York]"
        .parse()
        .expect("timestamp")
}

fn wait_params(reader: Id) -> MessageWaitToolParams {
    MessageWaitToolParams {
        room_id: None,
        after: None,
        limit: None,
        unread_only: None,
        timeout_seconds: Some(5),
        viewer: Some(format!("agent:{reader}")),
        principal: None,
        teams: Vec::new(),
    }
}

#[tokio::test]
async fn dropping_a_pending_message_wait_releases_admission_and_delivery_still_works() {
    let memory = Arc::new(
        Memory::open_in_memory(
            FakeEmbedder(EmbedderModel {
                family: "fake".to_string(),
                version: "1".to_string(),
                dimension: 2,
            }),
            &now(),
            MemoryConfig::default(),
        )
        .expect("memory"),
    );
    let notifier = Arc::new(MessageNotifier::default());
    let bounds = MessageWaitBounds {
        default_seconds: 5,
        max_seconds: 5,
        max_concurrent: 1,
        max_recipients: 256,
        heartbeat_seconds: 1,
    };
    let reader = Id::generate();
    let wait_memory = Arc::clone(&memory);
    let wait_notifier = Arc::clone(&notifier);
    let wait = tokio::spawn(async move {
        message_wait_tool_output(
            wait_memory.as_ref(),
            wait_notifier.as_ref(),
            wait_params(reader),
            None,
            AuthEnabled(false),
            bounds,
            None,
        )
        .await
    });
    tokio::time::timeout(Duration::from_millis(100), async {
        while notifier.live() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual message_wait reaches pending");

    wait.abort();
    let _ = wait.await;
    assert_eq!(notifier.live(), 0, "cancellation drops the wait ticket");

    let sender = Id::generate();
    message_send_tool_output(
        memory.as_ref(),
        notifier.as_ref(),
        MessageSendToolParams {
            to: format!("agent:{reader}"),
            body: "delivery after cancellation".to_string(),
            room_id: None,
            thread_id: None,
            reply_to_id: None,
            msg_kind: None,
            viewer: Some(format!("agent:{sender}")),
            principal: None,
            teams: Vec::new(),
        },
        &now(),
        None,
        AuthEnabled(false),
    )
    .expect("send after cancellation");
    let output = tokio::time::timeout(
        Duration::from_secs(1),
        message_wait_tool_output(
            memory.as_ref(),
            notifier.as_ref(),
            wait_params(reader),
            None,
            AuthEnabled(false),
            bounds,
            None,
        ),
    )
    .await
    .expect("subsequent wait returns promptly")
    .expect("subsequent wait succeeds");
    assert!(output.text.contains("delivery after cancellation"));
    assert!(output.text.contains("timed_out=false"));
}
