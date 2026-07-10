//! Durable, addressed agent-to-agent messages.
//!
//! A [`Message`] is an inbox record rather than retrievable memory. Like a
//! [`crate::nodes::work::WorkItem`], it composes only [`Identity`] (no
//! [`crate::blocks::Stats`]), so generic decay and forgetting never treat it as a memory.
//! Delivery is represented by the message's namespace: a direct message lives in the
//! recipient agent namespace and a broadcast lives in the recipient team namespace.
//! Dedicated message readers, not recall, surface the body. The delivery envelope (namespace,
//! sender, recipient, pointers, body, kind, and sent time) is immutable once its audited send
//! commits; only read state and retention expiry move afterward.

use serde::{Deserialize, Serialize};

use crate::blocks::Identity;
use crate::ids::Id;
use crate::time::Timestamp;

/// The application-level purpose of a [`Message`].
///
/// This is a small substrate-owned vocabulary serialized as a bare snake-case value.
/// [`MessageKind::Note`] is the default for callers that do not classify a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// A task or implementation brief.
    Brief,
    /// A progress or completion report.
    Status,
    /// Review feedback or a review verdict.
    Review,
    /// An acknowledgement message.
    Ack,
    /// General durable correspondence.
    #[default]
    Note,
}

impl MessageKind {
    /// Stable serialized label for [`MessageKind::Brief`].
    pub const BRIEF_LABEL: &str = "brief";
    /// Stable serialized label for [`MessageKind::Status`].
    pub const STATUS_LABEL: &str = "status";
    /// Stable serialized label for [`MessageKind::Review`].
    pub const REVIEW_LABEL: &str = "review";
    /// Stable serialized label for [`MessageKind::Ack`].
    pub const ACK_LABEL: &str = "ack";
    /// Stable serialized label for [`MessageKind::Note`].
    pub const NOTE_LABEL: &str = "note";
}

/// The recipient's read lifecycle for a [`Message`].
///
/// Stored under the dedicated `read_state` property, never the generic `status`
/// property used by fact lifecycle code. New messages start [`MessageReadState::Unread`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageReadState {
    /// Delivered but not yet marked read.
    #[default]
    Unread,
    /// Read by a recipient, but not explicitly acknowledged.
    Read,
    /// Explicitly acknowledged by a recipient.
    Acked,
}

impl MessageReadState {
    /// Stable serialized label for [`MessageReadState::Unread`].
    pub const UNREAD_LABEL: &str = "unread";
    /// Stable serialized label for [`MessageReadState::Read`].
    pub const READ_LABEL: &str = "read";
    /// Stable serialized label for [`MessageReadState::Acked`].
    pub const ACKED_LABEL: &str = "acked";
}

/// A durable, addressed message in an agent or team inbox.
///
/// `sender_id` is the authenticated author stamped by the message write path. `recipient`
/// is the canonical inbox namespace (`agent:<uuid>` or `team:<name>`) and must equal the
/// canonical string form of [`Identity::namespace`]. Room, thread, and reply ids are indexed
/// scalar pointers rather than graph edges. The whole delivery envelope is immutable after
/// send. `sent_at` is event time, while [`Identity::ingested_at`] is transaction time and the
/// keyset ordering axis. Only `read_state` and [`Identity::expired_at`] are mutable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Shared identity block; its namespace is the recipient inbox.
    pub identity: Identity,
    /// Authenticated author of this message.
    pub sender_id: Id,
    /// Canonical addressee (`agent:<uuid>` or `team:<name>`).
    pub recipient: String,
    /// Optional room/session grouping.
    pub room_id: Option<Id>,
    /// Optional thread grouping.
    pub thread_id: Option<Id>,
    /// Optional pointer to the message this replies to.
    pub reply_to_id: Option<Id>,
    /// Message body. It remains untrusted data when rendered to an agent.
    pub body: String,
    /// Application-level message purpose.
    pub msg_kind: MessageKind,
    /// Recipient read lifecycle.
    pub read_state: MessageReadState,
    /// Immutable event time at which the sender sent the message.
    pub sent_at: Timestamp,
}

impl Message {
    /// The selene-db node label for this kind.
    pub const LABEL: &str = "Message";
}
