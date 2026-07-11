//! In-process wakeups for bounded message long-polls.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use aionforge_config::MessagesConfig;
use rmcp::RoleServer;
use rmcp::model::{ProgressNotificationParam, ProgressToken};
use rmcp::service::{Peer, RequestContext};
use tokio::sync::Notify;

const MAX_RECIPIENT_KEY_BYTES_PER_WAIT: usize = 64 * 1024;
const HEARTBEAT_SEND_TIMEOUT: Duration = Duration::from_secs(2);

/// Runtime bounds for `message_wait`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageWaitBounds {
    /// Wait used when a caller omits `timeout_seconds`.
    pub default_seconds: u64,
    /// Hard server-side clamp for a requested wait.
    pub max_seconds: u64,
    /// Maximum number of concurrently parked waits.
    pub max_concurrent: usize,
    /// Maximum canonical recipient keys registered by one parked wait.
    pub max_recipients: usize,
    /// Cadence for opt-in progress heartbeats while a wait is parked.
    pub heartbeat_seconds: u64,
}

impl Default for MessageWaitBounds {
    fn default() -> Self {
        Self::from(&MessagesConfig::default())
    }
}

impl From<&MessagesConfig> for MessageWaitBounds {
    fn from(config: &MessagesConfig) -> Self {
        Self {
            default_seconds: config.wait_default_seconds,
            max_seconds: config.wait_max_seconds,
            max_concurrent: config.wait_max_concurrent,
            max_recipients: config.wait_max_recipients,
            heartbeat_seconds: config.wait_heartbeat_seconds,
        }
    }
}

impl MessageWaitBounds {
    /// Resolve one requested timeout, silently clamped to the configured bounds.
    pub(crate) fn resolve_seconds(self, requested: Option<u64>) -> u64 {
        let max = self.max_seconds.max(1);
        requested.unwrap_or(self.default_seconds).clamp(1, max)
    }

    /// Resolve a safe interval for opted-in progress notifications.
    pub(crate) fn heartbeat_period(self) -> Duration {
        Duration::from_secs(self.heartbeat_seconds.clamp(1, 86_400))
    }
}

/// Handler-side progress context that is retained only for an opted-in parked wait.
pub(crate) struct HeartbeatSink {
    peer: Peer<RoleServer>,
    token: ProgressToken,
    period: Duration,
}

impl HeartbeatSink {
    pub(crate) fn new(peer: Peer<RoleServer>, token: ProgressToken, period: Duration) -> Self {
        Self {
            peer,
            token,
            period,
        }
    }

    pub(crate) fn start(self, total_seconds: f64, started: tokio::time::Instant) -> Heartbeat {
        Heartbeat {
            peer: self.peer,
            token: self.token,
            period: self.period,
            total_seconds,
            started,
        }
    }
}

/// Build a progress sink only when this transport can deliver opted-in heartbeats.
pub(crate) fn message_wait_heartbeat_sink(
    context: &RequestContext<RoleServer>,
    bounds: MessageWaitBounds,
    heartbeats_enabled: bool,
) -> Option<HeartbeatSink> {
    heartbeats_enabled.then(|| {
        context
            .meta
            .get_progress_token()
            .map(|token| HeartbeatSink::new(context.peer.clone(), token, bounds.heartbeat_period()))
    })?
}

/// Best-effort progress side channel for one parked wait.
pub(crate) struct Heartbeat {
    peer: Peer<RoleServer>,
    token: ProgressToken,
    period: Duration,
    total_seconds: f64,
    started: tokio::time::Instant,
}

impl Heartbeat {
    async fn emit(&self, deadline: tokio::time::Instant) {
        let ceiling = (self.total_seconds - 0.5).max(0.0);
        let elapsed = self.started.elapsed().as_secs_f64().min(ceiling);
        let params = ProgressNotificationParam::new(self.token.clone(), elapsed)
            .with_total(self.total_seconds)
            .with_message("still waiting; 0 new");
        let send_bound = HEARTBEAT_SEND_TIMEOUT
            .min(self.period)
            .min(deadline.saturating_duration_since(tokio::time::Instant::now()));
        if send_bound.is_zero() {
            return;
        }
        send_heartbeat(send_bound, self.peer.notify_progress(params)).await;
    }
}

async fn send_heartbeat(
    send_bound: Duration,
    send: impl Future<Output = Result<(), rmcp::ServiceError>>,
) {
    match tokio::time::timeout(send_bound, send).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => tracing::debug!("message_wait heartbeat send failed; ignoring"),
        Err(_) => tracing::debug!("message_wait heartbeat send timed out; ignoring"),
    }
}

/// One registry shared by every MCP session served by a process.
#[derive(Default)]
pub(crate) struct MessageNotifier {
    waiters: Mutex<HashMap<String, Vec<Weak<Notify>>>>,
    live: AtomicUsize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitRegistrationError {
    ConcurrentLimit,
    RecipientLimit,
}

impl MessageNotifier {
    /// Register one per-call notifier under every visible recipient key.
    pub(crate) fn register(
        &self,
        recipients: &[String],
        max_concurrent: usize,
        max_recipients: usize,
    ) -> Result<WaitTicket<'_>, WaitRegistrationError> {
        let recipient_bytes = recipients
            .iter()
            .try_fold(0usize, |total, recipient| {
                total.checked_add(recipient.len())
            })
            .ok_or(WaitRegistrationError::RecipientLimit)?;
        if recipients.len() > max_recipients || recipient_bytes > MAX_RECIPIENT_KEY_BYTES_PER_WAIT {
            return Err(WaitRegistrationError::RecipientLimit);
        }
        self.live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                (live < max_concurrent).then_some(live + 1)
            })
            .map_err(|_| WaitRegistrationError::ConcurrentLimit)?;

        let notify = Arc::new(Notify::new());
        let weak = Arc::downgrade(&notify);
        let mut waiters = self
            .waiters
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for recipient in recipients {
            waiters
                .entry(recipient.clone())
                .or_default()
                .push(Weak::clone(&weak));
        }
        drop(waiters);

        Ok(WaitTicket {
            notifier: self,
            notify,
            weak,
            recipients: recipients.to_vec(),
        })
    }

    /// Wake all live waiters registered for one canonical recipient key.
    pub(crate) fn signal(&self, recipient: &str) {
        let mut waiters = self
            .waiters
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let remove = if let Some(registered) = waiters.get_mut(recipient) {
            registered.retain(|weak| {
                if let Some(notify) = weak.upgrade() {
                    notify.notify_waiters();
                    true
                } else {
                    false
                }
            });
            registered.is_empty()
        } else {
            false
        };
        if remove {
            waiters.remove(recipient);
        }
    }

    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }
}

/// RAII registration guard; dropping a request cancels and unregisters its wait.
pub(crate) struct WaitTicket<'a> {
    notifier: &'a MessageNotifier,
    notify: Arc<Notify>,
    weak: Weak<Notify>,
    recipients: Vec<String>,
}

impl WaitTicket<'_> {
    #[cfg(test)]
    pub(crate) fn notify(&self) -> &Notify {
        &self.notify
    }

    /// Arm before each deciding poll, then sleep until signaled or the deadline expires.
    pub(crate) async fn wait_until<T, E>(
        &self,
        deadline: tokio::time::Instant,
        heartbeat: Option<Heartbeat>,
        mut poll: impl FnMut() -> Result<Option<T>, E>,
    ) -> Result<Option<T>, E> {
        let mut ticker = heartbeat.as_ref().and_then(|heartbeat| {
            tokio::time::Instant::now()
                .checked_add(heartbeat.period)
                .map(|first_tick| {
                    let mut ticker = tokio::time::interval_at(first_tick, heartbeat.period);
                    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    ticker
                })
        });

        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if let Some(value) = poll()? {
                return Ok(Some(value));
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }

            match ticker.as_mut() {
                None => {
                    if tokio::time::timeout(remaining, notified).await.is_err() {
                        return Ok(None);
                    }
                }
                Some(ticker) => {
                    tokio::select! {
                        biased;
                        _ = &mut notified => {}
                        _ = tokio::time::sleep_until(deadline) => return Ok(None),
                        _ = ticker.tick() => {
                            if let Some(heartbeat) = heartbeat.as_ref() {
                                heartbeat.emit(deadline).await;
                            }
                            // A bounded send can consume the remaining deadline budget; do not
                            // make an expired post-tick poll change a timed-out result.
                            if deadline <= tokio::time::Instant::now() {
                                return Ok(None);
                            }
                        }
                    }
                }
            }
        }
    }
}

impl Drop for WaitTicket<'_> {
    fn drop(&mut self) {
        let mut waiters = self
            .notifier
            .waiters
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for recipient in &self.recipients {
            let remove = if let Some(registered) = waiters.get_mut(recipient) {
                registered.retain(|candidate| {
                    candidate.strong_count() > 0 && !candidate.ptr_eq(&self.weak)
                });
                registered.is_empty()
            } else {
                false
            };
            if remove {
                waiters.remove(recipient);
            }
        }
        drop(waiters);
        let _ = self
            .notifier
            .live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                live.checked_sub(1)
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_resolution_defaults_and_silently_clamps_both_edges() {
        let bounds = MessageWaitBounds {
            default_seconds: 3,
            max_seconds: 5,
            max_concurrent: 1,
            max_recipients: 2,
            heartbeat_seconds: 3,
        };
        assert_eq!(bounds.resolve_seconds(None), 3);
        assert_eq!(bounds.resolve_seconds(Some(0)), 1);
        assert_eq!(bounds.resolve_seconds(Some(99)), 5);
        assert_eq!(
            MessageWaitBounds {
                max_seconds: 0,
                ..bounds
            }
            .resolve_seconds(Some(2)),
            1,
        );
        assert_eq!(bounds.heartbeat_period(), Duration::from_secs(3));
        assert_eq!(
            MessageWaitBounds {
                heartbeat_seconds: 0,
                ..bounds
            }
            .heartbeat_period(),
            Duration::from_secs(1),
        );
        assert_eq!(
            MessageWaitBounds {
                heartbeat_seconds: 86_401,
                ..bounds
            }
            .heartbeat_period(),
            Duration::from_secs(86_400),
        );
    }

    #[test]
    fn registration_is_admission_bounded_and_drop_prunes_every_key() {
        let notifier = MessageNotifier::default();
        let recipients = vec!["agent:a".to_string(), "team:squad".to_string()];
        let first = notifier.register(&recipients, 1, 2).expect("first ticket");
        assert_eq!(notifier.live(), 1);
        assert!(matches!(
            notifier.register(&recipients, 1, 2),
            Err(WaitRegistrationError::ConcurrentLimit),
        ));

        drop(first);
        assert_eq!(notifier.live(), 0);
        let waiters = notifier
            .waiters
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert!(waiters.is_empty());
    }

    #[test]
    fn registration_bounds_persistent_recipient_keys_and_bytes() {
        let notifier = MessageNotifier::default();
        let too_many = (0..=256)
            .map(|index| format!("team:{index}"))
            .collect::<Vec<_>>();
        assert!(matches!(
            notifier.register(&too_many, 1, 256),
            Err(WaitRegistrationError::RecipientLimit),
        ));
        assert_eq!(notifier.live(), 0);

        let too_large = vec!["x".repeat(MAX_RECIPIENT_KEY_BYTES_PER_WAIT + 1)];
        assert!(matches!(
            notifier.register(&too_large, 1, 1),
            Err(WaitRegistrationError::RecipientLimit),
        ));
        assert_eq!(notifier.live(), 0);
    }

    #[tokio::test]
    async fn signal_is_recipient_keyed_and_wakes_every_matching_waiter() {
        let notifier = MessageNotifier::default();
        let a = vec!["agent:a".to_string()];
        let b = vec!["agent:b".to_string()];
        let a1 = notifier.register(&a, 3, 1).expect("a1");
        let a2 = notifier.register(&a, 3, 1).expect("a2");
        let b1 = notifier.register(&b, 3, 1).expect("b1");

        let a1_notified = a1.notify().notified();
        let a2_notified = a2.notify().notified();
        let b1_notified = b1.notify().notified();
        tokio::pin!(a1_notified, a2_notified, b1_notified);
        a1_notified.as_mut().enable();
        a2_notified.as_mut().enable();
        b1_notified.as_mut().enable();

        notifier.signal("agent:a");
        tokio::time::timeout(std::time::Duration::from_millis(50), a1_notified)
            .await
            .expect("a1 wakes");
        tokio::time::timeout(std::time::Duration::from_millis(50), a2_notified)
            .await
            .expect("a2 wakes");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), b1_notified)
                .await
                .is_err(),
            "a signal must not wake b",
        );
    }

    #[tokio::test]
    async fn aborting_a_parked_wait_drops_its_ticket() {
        let notifier = Arc::new(MessageNotifier::default());
        let task_notifier = Arc::clone(&notifier);
        let task = tokio::spawn(async move {
            let recipients = vec!["agent:a".to_string()];
            let ticket = task_notifier.register(&recipients, 1, 1).expect("ticket");
            ticket.notify().notified().await;
        });
        tokio::time::timeout(std::time::Duration::from_millis(50), async {
            while notifier.live() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("wait reaches pending with a live ticket");

        task.abort();
        let _ = task.await;
        assert_eq!(notifier.live(), 0);
        assert!(
            notifier.register(&["agent:a".to_string()], 1, 1).is_ok(),
            "aborting the waiter releases its admission slot",
        );
    }

    #[tokio::test]
    async fn arm_before_poll_closes_the_empty_snapshot_wakeup_race() {
        let notifier = MessageNotifier::default();
        let recipients = vec!["agent:a".to_string()];
        let ticket = notifier.register(&recipients, 1, 1).expect("ticket");
        let mut polls = 0;
        let found = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            ticket.wait_until(
                tokio::time::Instant::now() + std::time::Duration::from_secs(1),
                None,
                || -> Result<Option<&'static str>, ()> {
                    polls += 1;
                    if polls == 1 {
                        // The deciding snapshot was empty. Signal synchronously before the poll
                        // returns, which is before `wait_until` reaches its `.await`.
                        notifier.signal("agent:a");
                        Ok(None)
                    } else {
                        Ok(Some("committed message"))
                    }
                },
            ),
        )
        .await
        .expect("an armed waiter observes the between-poll-and-await signal")
        .expect("poll succeeds");
        assert_eq!(found, Some("committed message"));
        assert_eq!(polls, 2);
    }

    #[tokio::test]
    async fn heartbeat_send_failures_and_timeouts_are_ignored() {
        send_heartbeat(
            Duration::from_millis(1),
            std::future::ready(Err(rmcp::ServiceError::TransportClosed)),
        )
        .await;
        tokio::time::timeout(
            Duration::from_millis(50),
            send_heartbeat(
                Duration::from_millis(1),
                std::future::pending::<Result<(), rmcp::ServiceError>>(),
            ),
        )
        .await
        .expect("timed-out progress sends must not block the wait loop");
    }
}
