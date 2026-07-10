//! Durable-message retention configuration.
//!
//! Messages are identity-tier delivery records rather than memories, so their lifecycle is
//! governed by a dedicated ack-aware retention reaper instead of decay or the generic forgetting
//! sweeps. This block holds plain primitives only; the serving host maps the day windows into the
//! message-maintenance loop.
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
//! ```

use serde::{Deserialize, Serialize};

/// Ack-aware retention posture for first-class durable messages.
///
/// The default-on reaper retains acknowledged messages for 30 days and unread/read messages for
/// 90 days. A serving host may disable the reaper without changing either configured window by
/// setting [`retention_enabled`](Self::retention_enabled) to `false`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MessagesConfig {
    /// Whether the serving process runs the dedicated message-retention sweep.
    pub retention_enabled: bool,
    /// How many days acknowledged messages remain live after their event time.
    pub retention_acked_days: u64,
    /// How many days unread or read-but-unacknowledged messages remain live after their event time.
    pub retention_unacked_days: u64,
}

impl Default for MessagesConfig {
    fn default() -> Self {
        Self {
            retention_enabled: true,
            retention_acked_days: 30,
            retention_unacked_days: 90,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_ack_aware_retention_posture() {
        let config = MessagesConfig::default();
        assert!(config.retention_enabled);
        assert_eq!(config.retention_acked_days, 30);
        assert_eq!(config.retention_unacked_days, 90);
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
        };
        let json = serde_json::to_string(&config).expect("serialize");
        let back: MessagesConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, config);
    }
}
