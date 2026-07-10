//! Durable-message retention and long-poll configuration.
//!
//! Messages are identity-tier delivery records rather than memories, so their lifecycle is
//! governed by a dedicated ack-aware retention reaper instead of decay or the generic forgetting
//! sweeps. The same serving block bounds `message_wait` duration and concurrency. This block holds
//! plain primitives only; the serving host maps them into the message-maintenance loop and MCP
//! runtime.
//!
//! # Example
//!
//! A full `[messages]` block (every key is optional; shown with the defaults):
//!
//! ```toml
//! [messages]
//! retention_enabled = true
//! retention_acked_days = 30
//! retention_unacked_days = 90
//! wait_default_seconds = 25
//! wait_max_seconds = 55
//! wait_max_concurrent = 256
//! wait_max_recipients = 256
//! ```

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// Default maximum canonical recipient keys registered by one `message_wait` call.
pub const DEFAULT_WAIT_MAX_RECIPIENTS: usize = 256;

/// Ack-aware retention and bounded-wait posture for first-class durable messages.
///
/// The default-on reaper retains acknowledged messages for 30 days and unread/read messages for
/// 90 days. A serving host may disable the reaper without changing either configured window by
/// setting [`retention_enabled`](Self::retention_enabled) to `false`. Long-polls default to 25
/// seconds, are clamped to 55 seconds, and admit at most 256 concurrently parked calls with 256
/// canonical recipient keys apiece.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MessagesConfig {
    /// Whether the serving process runs the dedicated message-retention sweep.
    pub retention_enabled: bool,
    /// How many days acknowledged messages remain live after their event time.
    pub retention_acked_days: u64,
    /// How many days unread or read-but-unacknowledged messages remain live after their event time.
    pub retention_unacked_days: u64,
    /// Server-side wait used when `message_wait` omits `timeout_seconds`.
    pub wait_default_seconds: u64,
    /// Hard server-side clamp for one `message_wait` call; keep below client transport timeouts.
    pub wait_max_seconds: u64,
    /// Maximum concurrently parked waits; excess calls return an immediate timed-out empty page.
    pub wait_max_concurrent: usize,
    /// Maximum canonical recipient keys one wait may register before it becomes a one-shot read.
    pub wait_max_recipients: usize,
}

impl Default for MessagesConfig {
    fn default() -> Self {
        Self {
            retention_enabled: true,
            retention_acked_days: 30,
            retention_unacked_days: 90,
            wait_default_seconds: 25,
            wait_max_seconds: 55,
            wait_max_concurrent: 256,
            wait_max_recipients: DEFAULT_WAIT_MAX_RECIPIENTS,
        }
    }
}

impl MessagesConfig {
    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        if self.wait_max_seconds == 0 {
            return Err(ConfigError::invalid(
                "messages.wait_max_seconds",
                "must be at least 1",
            ));
        }
        if self.wait_default_seconds == 0 || self.wait_default_seconds > self.wait_max_seconds {
            return Err(ConfigError::invalid(
                "messages.wait_default_seconds",
                "must be in the range 1..=messages.wait_max_seconds",
            ));
        }
        if self.wait_max_concurrent == 0 {
            return Err(ConfigError::invalid(
                "messages.wait_max_concurrent",
                "must be at least 1",
            ));
        }
        if self.wait_max_recipients == 0 {
            return Err(ConfigError::invalid(
                "messages.wait_max_recipients",
                "must be at least 1",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_ack_aware_and_bound_message_waits() {
        let config = MessagesConfig::default();
        assert!(config.retention_enabled);
        assert_eq!(config.retention_acked_days, 30);
        assert_eq!(config.retention_unacked_days, 90);
        assert_eq!(config.wait_default_seconds, 25);
        assert_eq!(config.wait_max_seconds, 55);
        assert_eq!(config.wait_max_concurrent, 256);
        assert_eq!(config.wait_max_recipients, DEFAULT_WAIT_MAX_RECIPIENTS);
    }

    #[test]
    fn an_absent_block_is_the_default() {
        let parsed: MessagesConfig =
            serde_json::from_str("{}").expect("empty object parses via serde(default)");
        assert_eq!(parsed, MessagesConfig::default());
    }

    #[test]
    fn serde_round_trip_keeps_the_public_field_names() {
        let config = MessagesConfig {
            retention_enabled: false,
            retention_acked_days: 7,
            retention_unacked_days: 21,
            wait_default_seconds: 3,
            wait_max_seconds: 9,
            wait_max_concurrent: 12,
            wait_max_recipients: 24,
        };
        let json = serde_json::to_string(&config).expect("serialize");
        let back: MessagesConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, config);
    }

    #[test]
    fn validation_rejects_invalid_wait_bounds() {
        let config = MessagesConfig {
            wait_max_seconds: 0,
            ..MessagesConfig::default()
        };
        assert!(config.validate().is_err());

        let config = MessagesConfig {
            wait_default_seconds: 0,
            ..MessagesConfig::default()
        };
        assert!(config.validate().is_err());

        let defaults = MessagesConfig::default();
        let config = MessagesConfig {
            wait_default_seconds: defaults.wait_max_seconds + 1,
            ..defaults
        };
        assert!(config.validate().is_err());

        let config = MessagesConfig {
            wait_max_concurrent: 0,
            ..MessagesConfig::default()
        };
        assert!(config.validate().is_err());

        let config = MessagesConfig {
            wait_max_recipients: 0,
            ..MessagesConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
