//! Serve-owned scheduling for the dedicated durable-message retention sweep.
//!
//! Message expiry is independent of consolidation and generic forgetting. A serving process
//! performs one immediate sweep, then one sweep per hour, until shutdown. The single task awaits
//! each synchronous store commit before polling the next tick, so sweeps never overlap.

use std::sync::Arc;
use std::time::Duration;

use aionforge::{Embedder, Memory, Timestamp};
use aionforge_config::MessagesConfig;
use tokio::time::MissedTickBehavior;

const MESSAGE_RETENTION_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Handle for the serve-owned message-retention loop.
pub(crate) struct MessageRetentionHandle {
    shutdown: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl MessageRetentionHandle {
    /// Signal shutdown and wait for any in-flight sweep to commit before returning.
    pub(crate) async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

/// Start the default-on retention loop described by `[messages]`.
///
/// Returns `None` when retention is disabled. The first interval tick is immediate; later ticks
/// use the fixed hourly cadence with missed ticks skipped rather than replayed in a burst.
pub(crate) fn start<E: Embedder + 'static>(
    memory: Arc<Memory<E>>,
    config: &MessagesConfig,
) -> Option<MessageRetentionHandle> {
    if !config.retention_enabled {
        tracing::info!(
            target: "aionforge::serve",
            enabled = false,
            "background message retention disabled",
        );
        return None;
    }

    let retention_acked_days = config.retention_acked_days;
    let retention_unacked_days = config.retention_unacked_days;
    tracing::info!(
        target: "aionforge::serve",
        enabled = true,
        retention_acked_days,
        retention_unacked_days,
        tick_interval_secs = MESSAGE_RETENTION_INTERVAL.as_secs(),
        "background message retention enabled",
    );

    let (shutdown, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        // `interval` yields its first tick immediately. There is one task and each sweep is
        // completed inline before the next `select!`, which makes overlap impossible.
        let mut interval = tokio::time::interval(MESSAGE_RETENTION_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = interval.tick() => {
                    let now = Timestamp::now();
                    match memory.reap_messages(
                        &now,
                        retention_acked_days,
                        retention_unacked_days,
                    ) {
                        Ok(report) => tracing::info!(
                            target: "aionforge::serve",
                            acked_expired = report.acked_expired,
                            unacked_expired = report.unacked_expired,
                            total_expired = report.total(),
                            "message retention sweep completed",
                        ),
                        Err(error) => tracing::error!(
                            target: "aionforge::serve",
                            %error,
                            "message retention sweep failed",
                        ),
                    }
                }
            }
        }
    });

    Some(MessageRetentionHandle { shutdown, task })
}

#[cfg(test)]
mod tests {
    use std::future::Future;

    use aionforge::{
        EmbedderModel, Embedding, Id, Identity, MemoryConfig, Message, MessageKind,
        MessageReadState, Namespace, instant_before,
    };

    use super::*;

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
    struct NeverFails;

    impl std::fmt::Display for NeverFails {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("unreachable")
        }
    }

    impl std::error::Error for NeverFails {}

    impl Embedder for FakeEmbedder {
        type Error = NeverFails;

        fn embed(
            &self,
            inputs: &[String],
        ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
            let embeddings = inputs
                .iter()
                .map(|_| Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("valid embedding"))
                .collect();
            async move { Ok(embeddings) }
        }

        fn model(&self) -> &EmbedderModel {
            &self.model
        }
    }

    fn test_memory(now: &Timestamp) -> Arc<Memory<FakeEmbedder>> {
        Arc::new(
            Memory::open_in_memory(FakeEmbedder::new(), now, MemoryConfig::default())
                .expect("open test memory"),
        )
    }

    fn save_message(
        memory: &Memory<FakeEmbedder>,
        now: &Timestamp,
        sent_at: Timestamp,
        read_state: MessageReadState,
    ) -> Id {
        let sender_id = Id::generate();
        let recipient_id = Id::generate();
        let namespace = Namespace::Agent(recipient_id.to_string());
        let message = Message {
            identity: Identity {
                id: Id::generate(),
                ingested_at: now.clone(),
                namespace: namespace.clone(),
                expired_at: None,
            },
            sender_id,
            recipient: namespace.to_string(),
            room_id: None,
            thread_id: None,
            reply_to_id: None,
            body: "retention fixture".to_string(),
            msg_kind: MessageKind::Note,
            read_state: MessageReadState::Unread,
            sent_at,
        };
        memory
            .store()
            .save_message(&message, &sender_id, now)
            .expect("save message fixture");
        if read_state != MessageReadState::Unread {
            memory
                .store()
                .set_message_read_state(
                    &message.identity.id,
                    read_state,
                    Some(MessageReadState::Unread),
                    &recipient_id,
                    now,
                )
                .expect("advance message fixture through the read-state lifecycle");
        }
        message.identity.id
    }

    #[tokio::test]
    async fn disabled_retention_does_not_start_or_expire_messages() {
        let now = Timestamp::now();
        let memory = test_memory(&now);
        let old_acked = save_message(
            memory.as_ref(),
            &now,
            instant_before(&now, 31 * 86_400),
            MessageReadState::Acked,
        );
        let config = MessagesConfig {
            retention_enabled: false,
            ..MessagesConfig::default()
        };

        assert!(start(Arc::clone(&memory), &config).is_none());
        let stored = memory
            .store()
            .message_by_id(&old_acked)
            .expect("read old message")
            .expect("old message exists");
        assert!(stored.identity.expired_at.is_none());
    }

    #[tokio::test]
    async fn enabled_retention_reaps_immediately_and_keeps_current_unacked_message() {
        let now = Timestamp::now();
        let memory = test_memory(&now);
        let old_acked = save_message(
            memory.as_ref(),
            &now,
            instant_before(&now, 31 * 86_400),
            MessageReadState::Acked,
        );
        let current_unacked =
            save_message(memory.as_ref(), &now, now.clone(), MessageReadState::Unread);

        let handle = start(Arc::clone(&memory), &MessagesConfig::default())
            .expect("default config starts message retention");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let stored = memory
                    .store()
                    .message_by_id(&old_acked)
                    .expect("read old message")
                    .expect("old message exists");
                if stored.identity.expired_at.is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("immediate retention sweep completes");

        let current = memory
            .store()
            .message_by_id(&current_unacked)
            .expect("read current message")
            .expect("current message exists");
        assert!(current.identity.expired_at.is_none());
        handle.shutdown().await;
    }
}
