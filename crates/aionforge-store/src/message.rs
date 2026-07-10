//! Typed persistence for durable recipient-inbox messages.
//!
//! Messages are Identity-only inbox records, not retrievable memories. This module owns
//! their mechanical translation, audited send/read-state writes, keyset inbox reader, and
//! ack-aware retention sweep. Higher layers authenticate the sender and authorize an agent
//! or team recipient before calling [`Store::save_message`]; the store then enforces the
//! non-bypassable data invariants that the authenticated actor equals `sender_id` and the
//! message namespace equals its canonical recipient.

use std::cmp::Ordering;

use aionforge_domain::blocks::Identity;
use aionforge_domain::edges::Audit;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::message::{Message, MessageReadState};
use aionforge_domain::time::{Timestamp, instant_before};
use selene_core::{
    DbString, LabelDiff, LabelSet, NodeId, PropertyDiff, PropertyMap, Value, db_string,
};
use selene_graph::{RowIndex, SeleneGraph};
use serde::{Deserialize, Serialize};

use crate::convert::{
    as_id, as_namespace, as_str, as_timestamp, enum_from_value, enum_value, id_value, key,
    namespace_value, string_value, timestamp_value,
};
use crate::error::StoreError;
use crate::store::Store;

const ID: &str = "id";
const INGESTED_AT: &str = "ingested_at";
const NAMESPACE: &str = "namespace";
const EXPIRED_AT: &str = "expired_at";
const SENDER_ID: &str = "sender_id";
const RECIPIENT: &str = "recipient";
const ROOM_ID: &str = "room_id";
const THREAD_ID: &str = "thread_id";
const REPLY_TO_ID: &str = "reply_to_id";
const BODY: &str = "body";
const MSG_KIND: &str = "msg_kind";
const READ_STATE: &str = "read_state";
const SENT_AT: &str = "sent_at";

/// Maximum rows returned by one recipient-inbox page.
pub const MAX_MESSAGE_PAGE: usize = 200;

/// A keyset position in the stable `(ingested_at, id)` inbox ordering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageCursor {
    /// Transaction-time key of the last message on the prior page.
    pub ingested_at: Timestamp,
    /// Stable id tiebreaker for messages ingested at the same instant.
    pub id: Id,
}

impl MessageCursor {
    /// Build the continuation cursor for `message`.
    #[must_use]
    pub fn of(message: &Message) -> Self {
        Self {
            ingested_at: message.identity.ingested_at.clone(),
            id: message.identity.id,
        }
    }
}

/// One recipient-inbox page plus its continuation cursor.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MessagePage {
    /// Messages in ascending `(ingested_at, id)` order.
    pub messages: Vec<Message>,
    /// Cursor for the last returned message when another matching row exists.
    pub next: Option<MessageCursor>,
}

/// Counts from one ack-aware retention sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MessageRetentionReport {
    /// Acknowledged messages expired under the shorter acknowledged window.
    pub acked_expired: u64,
    /// Unread or read messages expired under the unacknowledged window.
    pub unacked_expired: u64,
}

impl MessageRetentionReport {
    /// Total messages soft-expired by the sweep.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.acked_expired + self.unacked_expired
    }
}

pub(crate) fn label() -> Result<LabelSet, StoreError> {
    Ok(LabelSet::single(db_string(Message::LABEL)?))
}

pub(crate) fn to_node(message: &Message) -> Result<(LabelSet, PropertyMap), StoreError> {
    let mut pairs: Vec<(DbString, Value)> = Vec::with_capacity(13);
    pairs.push((key(ID)?, id_value(&message.identity.id)?));
    pairs.push((
        key(INGESTED_AT)?,
        timestamp_value(&message.identity.ingested_at),
    ));
    pairs.push((
        key(NAMESPACE)?,
        namespace_value(&message.identity.namespace)?,
    ));
    if let Some(expired_at) = &message.identity.expired_at {
        pairs.push((key(EXPIRED_AT)?, timestamp_value(expired_at)));
    }
    pairs.push((key(SENDER_ID)?, id_value(&message.sender_id)?));
    pairs.push((key(RECIPIENT)?, string_value(&message.recipient)?));
    if let Some(room_id) = &message.room_id {
        pairs.push((key(ROOM_ID)?, id_value(room_id)?));
    }
    if let Some(thread_id) = &message.thread_id {
        pairs.push((key(THREAD_ID)?, id_value(thread_id)?));
    }
    if let Some(reply_to_id) = &message.reply_to_id {
        pairs.push((key(REPLY_TO_ID)?, id_value(reply_to_id)?));
    }
    pairs.push((key(BODY)?, string_value(&message.body)?));
    pairs.push((key(MSG_KIND)?, enum_value(&message.msg_kind)?));
    pairs.push((key(READ_STATE)?, enum_value(&message.read_state)?));
    pairs.push((key(SENT_AT)?, timestamp_value(&message.sent_at)));
    Ok((label()?, PropertyMap::from_pairs(pairs)?))
}

pub(crate) fn from_properties(props: &PropertyMap) -> Result<Message, StoreError> {
    let get =
        |name: &str| -> Result<Option<&Value>, StoreError> { Ok(props.get(&db_string(name)?)) };
    let require = |name: &str| -> Result<&Value, StoreError> {
        get(name)?.ok_or_else(|| StoreError::decode(format!("missing required property `{name}`")))
    };
    Ok(Message {
        identity: Identity {
            id: as_id(require(ID)?)?,
            ingested_at: as_timestamp(require(INGESTED_AT)?)?,
            namespace: as_namespace(require(NAMESPACE)?)?,
            expired_at: get(EXPIRED_AT)?.map(as_timestamp).transpose()?,
        },
        sender_id: as_id(require(SENDER_ID)?)?,
        recipient: as_str(require(RECIPIENT)?)?.to_owned(),
        room_id: get(ROOM_ID)?.map(as_id).transpose()?,
        thread_id: get(THREAD_ID)?.map(as_id).transpose()?,
        reply_to_id: get(REPLY_TO_ID)?.map(as_id).transpose()?,
        body: as_str(require(BODY)?)?.to_owned(),
        msg_kind: enum_from_value(require(MSG_KIND)?)?,
        read_state: enum_from_value(require(READ_STATE)?)?,
        sent_at: as_timestamp(require(SENT_AT)?)?,
    })
}

fn message_node_id_in(snapshot: &SeleneGraph, id: &Id) -> Result<Option<NodeId>, StoreError> {
    let label = db_string(Message::LABEL)?;
    let prop = db_string(ID)?;
    let value = id_value(id)?;
    let Some(rows) = snapshot.nodes_with_property_eq(&label, &prop, &value) else {
        return Ok(None);
    };
    Ok(rows
        .iter()
        .find_map(|row| snapshot.node_id_for_row(RowIndex::new(row))))
}

fn delivery_namespace(recipient: &str) -> Result<Namespace, StoreError> {
    if let Some(raw_id) = recipient.strip_prefix("agent:") {
        let id = Id::parse(raw_id).map_err(|_| {
            StoreError::invariant("an agent message recipient must contain a valid UUID")
        })?;
        let namespace = Namespace::Agent(id.to_string());
        if namespace.to_string() != recipient {
            return Err(StoreError::invariant(
                "an agent message recipient must use the canonical UUID spelling",
            ));
        }
        return Ok(namespace);
    }
    if let Some(team) = recipient.strip_prefix("team:")
        && !team.is_empty()
    {
        let namespace = Namespace::Team(team.to_owned());
        if namespace.to_string() == recipient {
            return Ok(namespace);
        }
    }
    Err(StoreError::invariant(
        "a message recipient must be a canonical agent:<uuid> or team:<name> namespace",
    ))
}

fn validate_send(message: &Message, actor: &Id) -> Result<(), StoreError> {
    if message.sender_id != *actor {
        return Err(StoreError::invariant(
            "message sender_id must equal the authenticated actor",
        ));
    }
    if message.identity.expired_at.is_some() {
        return Err(StoreError::invariant(
            "a newly sent message must not already be expired",
        ));
    }
    if message.read_state != MessageReadState::Unread {
        return Err(StoreError::invariant(
            "a newly sent message must start with read_state unread",
        ));
    }
    let namespace = delivery_namespace(&message.recipient)?;
    if message.identity.namespace != namespace {
        return Err(StoreError::invariant(
            "message namespace must equal its canonical recipient inbox",
        ));
    }
    Ok(())
}

fn compare_key(message: &Message, cursor: &MessageCursor) -> Ordering {
    message
        .identity
        .ingested_at
        .timestamp()
        .cmp(&cursor.ingested_at.timestamp())
        .then_with(|| message.identity.id.cmp(&cursor.id))
}

fn compare_messages(left: &Message, right: &Message) -> Ordering {
    left.identity
        .ingested_at
        .timestamp()
        .cmp(&right.identity.ingested_at.timestamp())
        .then_with(|| left.identity.id.cmp(&right.identity.id))
}

fn is_forward_read_state_transition(from: MessageReadState, to: MessageReadState) -> bool {
    matches!(
        (from, to),
        (
            MessageReadState::Unread,
            MessageReadState::Read | MessageReadState::Acked
        ) | (MessageReadState::Read, MessageReadState::Acked)
    )
}

impl Store {
    /// Atomically deliver a message and co-commit its signed send audit.
    ///
    /// Higher layers own the deliberate recipient-inbox authorization exception. This
    /// persistence boundary still refuses a caller-stamped sender, a non-agent/team recipient,
    /// a namespace that differs from the recipient, or an initial state other than `Unread`.
    /// The audit payload contains only the authenticated sender and recipient, never the
    /// untrusted body.
    ///
    /// # Errors
    /// Returns [`StoreError::Invariant`] when a delivery invariant fails, or [`StoreError`]
    /// when translation, mutation, audit signing, or commit fails.
    pub fn save_message(
        &self,
        message: &Message,
        actor: &Id,
        at: &Timestamp,
    ) -> Result<NodeId, StoreError> {
        validate_send(message, actor)?;
        let (labels, props) = to_node(message)?;
        let mut txn = self.graph().begin_write();
        let message_node = {
            let mut mutator = txn.mutator();
            let message_node = mutator.create_node(labels, props)?;
            let audit = AuditEvent {
                identity: Identity {
                    id: Id::generate(),
                    ingested_at: at.clone(),
                    namespace: message.identity.namespace.clone(),
                    expired_at: None,
                },
                kind: AuditKind::MessageSend,
                subject_id: message.identity.id,
                actor_id: *actor,
                payload: serde_json::json!({
                    "sender_id": actor,
                    "recipient": message.recipient,
                }),
                signature: String::new(),
                occurred_at: at.clone(),
            };
            let ensured = crate::audit::ensure_event(&mut mutator, &audit, self.audit_signer())?;
            if ensured.created {
                mutator.create_edge(
                    db_string(Audit::LABEL)?,
                    ensured.node,
                    message_node,
                    PropertyMap::from_pairs(Vec::new())?,
                )?;
            }
            message_node
        };
        txn.commit()?;
        Ok(message_node)
    }

    /// Read a message by its stable domain id, including soft-expired rows.
    ///
    /// Caller-facing surfaces must still apply visibility and expiry policy before rendering.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the lookup or stored message decode fails.
    pub fn message_by_id(&self, id: &Id) -> Result<Option<Message>, StoreError> {
        let snapshot = self.graph().read();
        let Some(node) = message_node_id_in(&snapshot, id)? else {
            return Ok(None);
        };
        snapshot
            .node_properties(node)
            .map(from_properties)
            .transpose()
    }

    /// Read one live recipient inbox page in ascending `(ingested_at, id)` order.
    ///
    /// The exact recipient scalar is index-probed. Rows whose namespace does not equal that
    /// canonical recipient are dropped defensively, so malformed cross-namespace data cannot
    /// widen visibility. `room` and `unread_only` are optional filters; `after` is exclusive.
    /// `limit` is clamped to `1..=`[`MAX_MESSAGE_PAGE`].
    ///
    /// # Errors
    /// Returns [`StoreError`] if the recipient is not an agent/team inbox or a stored row cannot
    /// be decoded.
    pub fn messages_for_recipient(
        &self,
        recipient: &str,
        room: Option<&Id>,
        after: Option<&MessageCursor>,
        limit: usize,
        unread_only: bool,
    ) -> Result<MessagePage, StoreError> {
        let namespace = delivery_namespace(recipient)?;
        let snapshot = self.graph().read();
        let label = db_string(Message::LABEL)?;
        let property = db_string(RECIPIENT)?;
        let value = string_value(recipient)?;
        let Some(rows) = snapshot.nodes_with_property_eq(&label, &property, &value) else {
            return Ok(MessagePage::default());
        };
        let mut messages = Vec::new();
        for row in rows.iter() {
            let Some(node) = snapshot.node_id_for_row(RowIndex::new(row)) else {
                continue;
            };
            let Some(props) = snapshot.node_properties(node) else {
                continue;
            };
            let message = from_properties(props)?;
            if message.identity.expired_at.is_some()
                || message.identity.namespace != namespace
                || room.is_some_and(|room_id| message.room_id.as_ref() != Some(room_id))
                || (unread_only && message.read_state != MessageReadState::Unread)
                || after.is_some_and(|cursor| compare_key(&message, cursor) != Ordering::Greater)
            {
                continue;
            }
            messages.push(message);
        }
        messages.sort_by(compare_messages);
        let limit = limit.clamp(1, MAX_MESSAGE_PAGE);
        let has_more = messages.len() > limit;
        messages.truncate(limit);
        let next = has_more
            .then(|| messages.last().map(MessageCursor::of))
            .flatten();
        Ok(MessagePage { messages, next })
    }

    /// Change a message's read state with an optional compare-and-set guard and signed audit.
    ///
    /// The state update, `MessageReadStateChange` audit node, and `AUDIT` edge commit in one
    /// transaction. Only monotonic transitions are accepted: `Unread` may become `Read` or
    /// `Acked`, and `Read` may become `Acked`. A same-state request is an idempotent no-op and
    /// emits no audit. Expired messages reject every transition, including same-state requests.
    ///
    /// # Errors
    /// Returns [`StoreError::Invariant`] when `expected_from` is stale, the message is expired,
    /// or the requested transition is non-monotonic. Returns [`StoreError`] when no message
    /// carries `id`, or when mutation, audit signing, or commit fails.
    pub fn set_message_read_state(
        &self,
        id: &Id,
        to: MessageReadState,
        expected_from: Option<MessageReadState>,
        actor: &Id,
        at: &Timestamp,
    ) -> Result<Message, StoreError> {
        let mut txn = self.graph().begin_write();
        let updated = {
            let mut mutator = txn.mutator();
            let node = message_node_id_in(mutator.read(), id)?
                .ok_or_else(|| StoreError::decode(format!("message {id} not found")))?;
            let current = {
                let read = mutator.read();
                let props = read
                    .node_properties(node)
                    .ok_or_else(|| StoreError::decode("message vanished mid-transition"))?;
                from_properties(props)?
            };
            if current.identity.expired_at.is_some() {
                return Err(StoreError::invariant(format!(
                    "message {id} is expired and cannot change read_state"
                )));
            }
            let from = current.read_state;
            if let Some(expected) = expected_from
                && from != expected
            {
                return Err(StoreError::invariant(format!(
                    "message {id} is {from:?}, expected {expected:?} to advance to {to:?}"
                )));
            }
            if to == from {
                return Ok(current);
            }
            if !is_forward_read_state_transition(from, to) {
                return Err(StoreError::invariant(format!(
                    "message {id} read_state cannot move backward from {from:?} to {to:?}"
                )));
            }
            mutator.update_node(
                node,
                LabelDiff::new([], [])?,
                PropertyDiff::new([(db_string(READ_STATE)?, enum_value(&to)?)], [])?,
            )?;
            let audit = AuditEvent {
                identity: Identity {
                    id: Id::generate(),
                    ingested_at: at.clone(),
                    namespace: current.identity.namespace.clone(),
                    expired_at: None,
                },
                kind: AuditKind::MessageReadStateChange,
                subject_id: *id,
                actor_id: *actor,
                payload: serde_json::json!({ "from": from, "to": to }),
                signature: String::new(),
                occurred_at: at.clone(),
            };
            let ensured = crate::audit::ensure_event(&mut mutator, &audit, self.audit_signer())?;
            if ensured.created {
                mutator.create_edge(
                    db_string(Audit::LABEL)?,
                    ensured.node,
                    node,
                    PropertyMap::from_pairs(Vec::new())?,
                )?;
            }
            Message {
                read_state: to,
                ..current
            }
        };
        txn.commit()?;
        Ok(updated)
    }

    /// Soft-expire live messages older than their ack-aware retention windows.
    ///
    /// `Acked` rows use `retention_acked_days`; `Unread` and `Read` rows use
    /// `retention_unacked_days`. Age is measured from immutable `sent_at` in exact 86,400-second
    /// days. A row exactly on its cutoff is retained (expiry requires age to *exceed* the
    /// window). All qualifying rows are stamped with `now` in one commit.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a message cannot be decoded or the expiry commit fails.
    pub fn reap_messages(
        &self,
        now: &Timestamp,
        retention_acked_days: u64,
        retention_unacked_days: u64,
    ) -> Result<MessageRetentionReport, StoreError> {
        let acked_cutoff = instant_before(now, retention_acked_days.saturating_mul(86_400));
        let unacked_cutoff = instant_before(now, retention_unacked_days.saturating_mul(86_400));
        let mut txn = self.graph().begin_write();
        let mut report = MessageRetentionReport::default();
        let candidates = {
            let mutator = txn.mutator();
            let snapshot = mutator.read();
            let label = db_string(Message::LABEL)?;
            let Some(rows) = snapshot.nodes_with_label(&label) else {
                return Ok(report);
            };
            let mut candidates = Vec::new();
            for row in rows.iter() {
                let Some(node) = snapshot.node_id_for_row(RowIndex::new(row)) else {
                    continue;
                };
                let Some(props) = snapshot.node_properties(node) else {
                    continue;
                };
                let message = from_properties(props)?;
                if message.identity.expired_at.is_some() {
                    continue;
                }
                let cutoff = if message.read_state == MessageReadState::Acked {
                    &acked_cutoff
                } else {
                    &unacked_cutoff
                };
                if message.sent_at.timestamp() < cutoff.timestamp() {
                    candidates.push((node, message.read_state));
                }
            }
            candidates
        };
        if candidates.is_empty() {
            return Ok(report);
        }
        {
            let mut mutator = txn.mutator();
            for (node, state) in candidates {
                mutator.update_node(
                    node,
                    LabelDiff::new([], [])?,
                    PropertyDiff::new([(db_string(EXPIRED_AT)?, timestamp_value(now))], [])?,
                )?;
                if state == MessageReadState::Acked {
                    report.acked_expired += 1;
                } else {
                    report.unacked_expired += 1;
                }
            }
        }
        txn.commit()?;
        Ok(report)
    }
}
