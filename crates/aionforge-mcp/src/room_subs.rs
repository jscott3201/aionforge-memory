//! In-process subscriptions for room-resource update notifications.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use aionforge_config::MessagesConfig;
use rmcp::RoleServer;
use rmcp::model::ResourceUpdatedNotificationParam;
use rmcp::service::Peer;

const SEND_TIMEOUT: Duration = Duration::from_secs(2);

/// Runtime admission bound for room-resource subscriptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoomSubscribeBounds {
    /// Maximum live room subscriptions across every session served by this process.
    pub max_concurrent: usize,
}

impl Default for RoomSubscribeBounds {
    fn default() -> Self {
        Self::from(&MessagesConfig::default())
    }
}

impl From<&MessagesConfig> for RoomSubscribeBounds {
    fn from(config: &MessagesConfig) -> Self {
        Self {
            max_concurrent: config.room_subscribe_max_concurrent,
        }
    }
}

/// Per-session liveness marker. Its weak form lets the registry prune disconnected sessions.
#[derive(Default)]
pub(crate) struct SessionMarker;

struct Subscriber {
    marker: Weak<SessionMarker>,
    peer: Peer<RoleServer>,
    uri: String,
    recipients: Vec<String>,
    generation: u64,
}

struct Target {
    marker: Weak<SessionMarker>,
    peer: Peer<RoleServer>,
    uri: String,
    generation: u64,
}

/// Shared room-subscription dependencies installed into one MCP handler.
pub(crate) struct RoomSubscriptionRuntime {
    pub(crate) subscriptions: Arc<RoomSubscriptions>,
    pub(crate) bounds: RoomSubscribeBounds,
}

impl RoomSubscriptionRuntime {
    pub(crate) fn new(subscriptions: Arc<RoomSubscriptions>, bounds: RoomSubscribeBounds) -> Self {
        Self {
            subscriptions,
            bounds,
        }
    }
}

/// One room-subscription registry shared by every MCP session in a process.
#[derive(Default)]
pub(crate) struct RoomSubscriptions {
    rooms: Mutex<HashMap<String, Vec<Subscriber>>>,
    total: AtomicUsize,
    next_generation: AtomicU64,
}

/// Failure to admit a new room subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubscribeError {
    /// The process-wide subscription cap is already occupied.
    ConcurrentLimit,
}

impl RoomSubscriptions {
    /// Add or refresh one session's subscription to a room URI.
    pub(crate) fn subscribe(
        &self,
        room_id: &str,
        uri: String,
        marker: &Arc<SessionMarker>,
        peer: Peer<RoleServer>,
        recipients: Vec<String>,
        bounds: RoomSubscribeBounds,
    ) -> Result<(), SubscribeError> {
        let marker_weak = Arc::downgrade(marker);
        let mut rooms = self.rooms.lock().unwrap_or_else(|error| error.into_inner());
        self.decrement_total(prune_dead(&mut rooms));

        if let Some(subscribers) = rooms.get_mut(room_id)
            && let Some(existing) = subscribers
                .iter_mut()
                .find(|subscriber| subscriber.marker.ptr_eq(&marker_weak) && subscriber.uri == uri)
        {
            existing.peer = peer;
            existing.recipients = recipients;
            existing.generation = self.next_generation();
            return Ok(());
        }

        self.total
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |total| {
                (total < bounds.max_concurrent).then_some(total + 1)
            })
            .map_err(|_| SubscribeError::ConcurrentLimit)?;
        rooms
            .entry(room_id.to_string())
            .or_default()
            .push(Subscriber {
                marker: marker_weak,
                peer,
                uri,
                recipients,
                generation: self.next_generation(),
            });
        Ok(())
    }

    /// Remove one session's exact URI subscription. Unknown subscriptions are harmless.
    pub(crate) fn unsubscribe(&self, room_id: &str, uri: &str, marker: &Arc<SessionMarker>) {
        let marker_weak = Arc::downgrade(marker);
        let mut rooms = self.rooms.lock().unwrap_or_else(|error| error.into_inner());
        let removed = if let Some(subscribers) = rooms.get_mut(room_id) {
            let before = subscribers.len();
            subscribers.retain(|subscriber| {
                subscriber.marker.upgrade().is_some()
                    && !(subscriber.marker.ptr_eq(&marker_weak) && subscriber.uri == uri)
            });
            before - subscribers.len()
        } else {
            0
        };
        rooms.retain(|_, subscribers| !subscribers.is_empty());
        self.decrement_total(removed);
    }

    /// Emit an update hint to live subscribers whose recipient snapshot includes this message.
    pub(crate) async fn notify_room(&self, room_id: &str, recipient: &str) {
        let mut targets = Vec::new();
        let removed = {
            let mut rooms = self.rooms.lock().unwrap_or_else(|error| error.into_inner());
            let removed = if let Some(subscribers) = rooms.get_mut(room_id) {
                let before = subscribers.len();
                subscribers.retain(|subscriber| subscriber.marker.upgrade().is_some());
                targets.extend(
                    subscribers
                        .iter()
                        .filter(|subscriber| {
                            subscriber.recipients.iter().any(|key| key == recipient)
                        })
                        .map(|subscriber| Target {
                            marker: Weak::clone(&subscriber.marker),
                            peer: subscriber.peer.clone(),
                            uri: subscriber.uri.clone(),
                            generation: subscriber.generation,
                        }),
                );
                before - subscribers.len()
            } else {
                0
            };
            rooms.retain(|_, subscribers| !subscribers.is_empty());
            removed
        };
        self.decrement_total(removed);

        let mut dead = Vec::new();
        for target in targets {
            let result = tokio::time::timeout(
                SEND_TIMEOUT,
                target
                    .peer
                    .notify_resource_updated(ResourceUpdatedNotificationParam::new(&target.uri)),
            )
            .await;
            if !matches!(result, Ok(Ok(()))) {
                tracing::debug!("room resource update delivery failed; pruning subscription");
                dead.push((target.marker, target.uri, target.generation));
            }
        }

        if dead.is_empty() {
            return;
        }
        let mut rooms = self.rooms.lock().unwrap_or_else(|error| error.into_inner());
        let removed = if let Some(subscribers) = rooms.get_mut(room_id) {
            let before = subscribers.len();
            subscribers.retain(|subscriber| {
                !dead.iter().any(|(marker, uri, generation)| {
                    subscriber.marker.ptr_eq(marker)
                        && subscriber.uri == *uri
                        && subscriber.generation == *generation
                })
            });
            before - subscribers.len()
        } else {
            0
        };
        rooms.retain(|_, subscribers| !subscribers.is_empty());
        self.decrement_total(removed);
    }

    fn next_generation(&self) -> u64 {
        self.next_generation.fetch_add(1, Ordering::Relaxed)
    }

    fn decrement_total(&self, removed: usize) {
        if removed == 0 {
            return;
        }
        let _ = self
            .total
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |total| {
                total.checked_sub(removed)
            });
    }
}

fn prune_dead(rooms: &mut HashMap<String, Vec<Subscriber>>) -> usize {
    let mut removed = 0;
    rooms.retain(|_, subscribers| {
        let before = subscribers.len();
        subscribers.retain(|subscriber| subscriber.marker.upgrade().is_some());
        removed += before - subscribers.len();
        !subscribers.is_empty()
    });
    removed
}

#[cfg(test)]
mod tests {
    use super::RoomSubscribeBounds;
    use aionforge_config::MessagesConfig;

    #[test]
    fn room_subscription_bounds_follow_messages_config() {
        let bounds = RoomSubscribeBounds::from(&MessagesConfig::default());
        assert_eq!(bounds.max_concurrent, 256);
    }
}
