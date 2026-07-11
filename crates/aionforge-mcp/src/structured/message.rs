//! Typed `structuredContent` payloads for the agent-message MCP tools.

use aionforge_domain::nodes::message::Message;
use serde::Serialize;

use crate::inspect::SNIPPET_CHARS;
use crate::render::{message_kind_tag, message_read_state_tag};

/// Stable cursor shape shared by `message_poll` text and structured output.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct MessageCursorStructured {
    pub(crate) ingested_at: String,
    pub(crate) id: String,
}

/// One message envelope and bounded body preview returned by `message_poll`.
#[derive(Serialize)]
struct MessageStructured {
    id: String,
    namespace: String,
    sender_id: String,
    recipient: String,
    room_id: Option<String>,
    thread_id: Option<String>,
    reply_to_id: Option<String>,
    body: String,
    body_truncated: bool,
    msg_kind: &'static str,
    read_state: &'static str,
    sent_at: String,
    ingested_at: String,
}

/// `aionforge.message_send.v1` receipt.
#[derive(Serialize)]
pub(crate) struct MessageSendStructured {
    schema: &'static str,
    id: String,
    recipient: String,
    sent_at: String,
}

impl MessageSendStructured {
    pub(crate) fn new(message: &Message) -> Self {
        Self {
            schema: "aionforge.message_send.v1",
            id: message.identity.id.to_string(),
            recipient: message.recipient.clone(),
            sent_at: message.sent_at.to_string(),
        }
    }
}

/// `aionforge.message_poll.v1` page.
#[derive(Serialize)]
pub(crate) struct MessagePollStructured {
    schema: &'static str,
    count: usize,
    limit: usize,
    unread_only: bool,
    next: Option<MessageCursorStructured>,
    messages: Vec<MessageStructured>,
}

impl MessagePollStructured {
    pub(crate) fn new(
        messages: &[Message],
        limit: usize,
        unread_only: bool,
        next: Option<MessageCursorStructured>,
    ) -> Self {
        Self {
            schema: "aionforge.message_poll.v1",
            count: messages.len(),
            limit,
            unread_only,
            next,
            messages: messages.iter().map(MessageStructured::from).collect(),
        }
    }
}

/// `aionforge.message_wait.v1` page.
#[derive(Serialize)]
pub(crate) struct MessageWaitStructured {
    schema: &'static str,
    timed_out: bool,
    count: usize,
    limit: usize,
    unread_only: bool,
    next: Option<MessageCursorStructured>,
    messages: Vec<MessageStructured>,
}

impl MessageWaitStructured {
    pub(crate) fn new(
        messages: &[Message],
        limit: usize,
        unread_only: bool,
        next: Option<MessageCursorStructured>,
        timed_out: bool,
    ) -> Self {
        Self {
            schema: "aionforge.message_wait.v1",
            timed_out,
            count: messages.len(),
            limit,
            unread_only,
            next,
            messages: messages.iter().map(MessageStructured::from).collect(),
        }
    }
}

impl From<&Message> for MessageStructured {
    fn from(message: &Message) -> Self {
        let (body, body_truncated) = compact_body(&message.body);
        Self {
            id: message.identity.id.to_string(),
            namespace: message.identity.namespace.to_string(),
            sender_id: message.sender_id.to_string(),
            recipient: message.recipient.clone(),
            room_id: message.room_id.map(|id| id.to_string()),
            thread_id: message.thread_id.map(|id| id.to_string()),
            reply_to_id: message.reply_to_id.map(|id| id.to_string()),
            body,
            body_truncated,
            msg_kind: message_kind_tag(message.msg_kind),
            read_state: message_read_state_tag(message.read_state),
            sent_at: message.sent_at.to_string(),
            ingested_at: message.identity.ingested_at.to_string(),
        }
    }
}

fn compact_body(body: &str) -> (String, bool) {
    let mut chars = body.chars();
    let compact = chars.by_ref().take(SNIPPET_CHARS).collect();
    (compact, chars.next().is_some())
}

/// One per-id outcome returned by `message_ack`.
#[derive(Clone, Serialize)]
pub(crate) struct MessageAckOutcomeStructured {
    pub(crate) id: String,
    pub(crate) outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) from: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) to: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

/// `aionforge.message_ack.v1` batch receipt.
#[derive(Serialize)]
pub(crate) struct MessageAckStructured {
    schema: &'static str,
    requested: usize,
    updated: usize,
    unchanged: usize,
    not_found: usize,
    conflict: usize,
    failed: usize,
    outcomes: Vec<MessageAckOutcomeStructured>,
}

impl MessageAckStructured {
    pub(crate) fn new(outcomes: Vec<MessageAckOutcomeStructured>) -> Self {
        let requested = outcomes.len();
        let updated = count(&outcomes, "updated");
        let unchanged = count(&outcomes, "unchanged");
        let not_found = count(&outcomes, "not_found");
        let conflict = count(&outcomes, "conflict");
        let failed = requested - updated - unchanged - not_found - conflict;
        Self {
            schema: "aionforge.message_ack.v1",
            requested,
            updated,
            unchanged,
            not_found,
            conflict,
            failed,
            outcomes,
        }
    }
}

fn count(outcomes: &[MessageAckOutcomeStructured], outcome: &str) -> usize {
    outcomes
        .iter()
        .filter(|item| item.outcome == outcome)
        .count()
}

#[cfg(test)]
mod tests {
    use super::{MessageAckOutcomeStructured, MessageAckStructured};

    #[test]
    fn ack_rollup_keeps_conflicts_separate_from_failures() {
        let receipt = MessageAckStructured::new(vec![
            outcome("updated"),
            outcome("not_found"),
            outcome("conflict"),
            outcome("failed"),
        ]);
        let receipt = serde_json::to_value(receipt).expect("ack receipt serializes");

        assert_eq!(receipt["requested"].as_u64(), Some(4));
        assert_eq!(receipt["updated"].as_u64(), Some(1));
        assert_eq!(receipt["not_found"].as_u64(), Some(1));
        assert_eq!(receipt["conflict"].as_u64(), Some(1));
        assert_eq!(receipt["failed"].as_u64(), Some(1));
    }

    fn outcome(outcome: &'static str) -> MessageAckOutcomeStructured {
        MessageAckOutcomeStructured {
            id: "00000000-0000-0000-0000-000000000000".to_string(),
            outcome,
            from: None,
            to: None,
            error: None,
        }
    }
}
