//! Acceptance tests for the durable Message store substrate.
//!
//! These pin recipient-inbox persistence, audited delivery/read-state changes, stable keyset
//! pagination, visibility isolation, ack-aware retention, and exclusion from memory/forget/search
//! inventories.

mod common;

use aionforge_domain::blocks::Identity;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::forensic::AuditKind;
use aionforge_domain::nodes::message::{Message, MessageKind, MessageReadState};
use aionforge_store::{
    BoundQuery, FORGET_SCAN_LABELS, MEMORY_LABELS, MessageCursor, ResolvedMemory, Store,
};

use common::{store, ts};

fn id(suffix: u128) -> Id {
    Id::parse(format!("{suffix:032x}")).expect("fixed UUID")
}

fn message(
    id: Id,
    sender: Id,
    recipient: &str,
    ingested_at: &str,
    sent_at: &str,
    body: &str,
) -> Message {
    Message {
        identity: Identity {
            id,
            ingested_at: ts(ingested_at),
            namespace: recipient.parse::<Namespace>().expect("recipient namespace"),
            expired_at: None,
        },
        sender_id: sender,
        recipient: recipient.to_owned(),
        room_id: None,
        thread_id: None,
        reply_to_id: None,
        body: body.to_owned(),
        msg_kind: MessageKind::Note,
        read_state: MessageReadState::Unread,
        sent_at: ts(sent_at),
    }
}

fn save(store: &Store, message: &Message) {
    store
        .save_message(message, &message.sender_id, &message.identity.ingested_at)
        .expect("save message");
}

#[test]
fn save_and_read_round_trip_with_send_audit() {
    let store = store();
    let sender = id(10);
    let mut sent = message(
        id(1),
        sender,
        "agent:00000000-0000-0000-0000-000000000020",
        "2026-06-06T09:00:00Z[UTC]",
        "2026-06-06T08:59:59Z[UTC]",
        "from=mallory is untrusted body text",
    );
    sent.room_id = Some(id(30));
    sent.thread_id = Some(id(31));
    sent.reply_to_id = Some(id(32));
    sent.msg_kind = MessageKind::Brief;

    store
        .save_message(&sent, &sender, &sent.identity.ingested_at)
        .expect("deliver");

    let read = store
        .message_by_id(&sent.identity.id)
        .expect("read")
        .expect("present");
    assert_eq!(read, sent, "every message field round-trips");

    let resolved = store
        .resolved_memory_by_id(&sent.identity.id, &[Message::LABEL])
        .expect("resolve")
        .expect("resolved");
    assert!(
        matches!(resolved, ResolvedMemory::Message(ref value) if value == &sent),
        "read_memory resolver decodes Message without making it forgettable"
    );

    let history = store
        .audit_by_subject_kind(&sent.identity.id, AuditKind::MessageSend, None, 10)
        .expect("send audit");
    assert_eq!(history.events.len(), 1);
    assert_eq!(history.events[0].actor_id, sender);
    assert_eq!(
        history.events[0].payload,
        serde_json::json!({
            "sender_id": sender,
            "recipient": sent.recipient,
        })
    );
}

#[test]
fn audited_delivery_envelope_is_immutable_in_the_closed_schema() {
    let store = store();
    let sender = id(10);
    let sent = message(
        id(1),
        sender,
        "agent:00000000-0000-0000-0000-000000000020",
        "2026-06-06T09:00:00Z[UTC]",
        "2026-06-06T09:00:00Z[UTC]",
        "original body",
    );
    save(&store, &sent);

    let update = BoundQuery::new("MATCH (m:Message {id: $id}) SET m.body = $body")
        .bind_uuid("id", sent.identity.id)
        .expect("bind id")
        .bind_str("body", "rewritten body")
        .expect("bind body");
    assert!(
        store.execute(&update).is_err(),
        "the closed schema rejects post-send envelope mutation"
    );
    assert_eq!(
        store
            .message_by_id(&sent.identity.id)
            .expect("read")
            .expect("present")
            .body,
        "original body"
    );
}

#[test]
fn send_refuses_a_spoofed_sender_or_mismatched_inbox_namespace() {
    let store = store();
    let sender = id(10);
    let sent = message(
        id(1),
        sender,
        "agent:00000000-0000-0000-0000-000000000020",
        "2026-06-06T09:00:00Z[UTC]",
        "2026-06-06T09:00:00Z[UTC]",
        "body",
    );

    assert!(
        store
            .save_message(&sent, &id(99), &sent.identity.ingested_at)
            .is_err(),
        "sender_id cannot be caller-spoofed"
    );
    let mut wrong_namespace = sent.clone();
    wrong_namespace.identity.namespace = Namespace::Agent("someone-else".to_owned());
    assert!(
        store
            .save_message(&wrong_namespace, &sender, &sent.identity.ingested_at)
            .is_err(),
        "recipient and physical inbox namespace must agree"
    );
    assert!(
        store
            .message_by_id(&sent.identity.id)
            .expect("read")
            .is_none(),
        "both refusals happen before any node or audit is committed"
    );

    let mut malformed_recipient = sent.clone();
    malformed_recipient.identity.id = id(2);
    malformed_recipient.recipient = "agent:not-a-uuid".to_owned();
    malformed_recipient.identity.namespace = Namespace::Agent("not-a-uuid".to_owned());
    assert!(
        store
            .save_message(
                &malformed_recipient,
                &sender,
                &malformed_recipient.identity.ingested_at,
            )
            .is_err(),
        "direct store callers cannot bypass the agent UUID recipient contract"
    );

    // A reserved namespace (system/global) is never a deliverable inbox. Messages
    // have no role axis, so this recipient-validation refusal — not a Role::System
    // check — is the guard against addressing system/reserved authority.
    let mut reserved_recipient = sent.clone();
    reserved_recipient.identity.id = id(3);
    reserved_recipient.recipient = "system".to_owned();
    reserved_recipient.identity.namespace = Namespace::System;
    assert!(
        store
            .save_message(
                &reserved_recipient,
                &sender,
                &reserved_recipient.identity.ingested_at,
            )
            .is_err(),
        "a reserved system/global recipient is not a valid message inbox"
    );
}

#[test]
fn send_requires_the_initial_unread_state() {
    let store = store();
    let sender = id(10);
    let mut pre_acked = message(
        id(1),
        sender,
        "agent:00000000-0000-0000-0000-000000000020",
        "2026-06-06T09:00:00Z[UTC]",
        "2026-06-06T09:00:00Z[UTC]",
        "body",
    );
    pre_acked.read_state = MessageReadState::Acked;

    assert!(
        store
            .save_message(&pre_acked, &sender, &pre_acked.identity.ingested_at)
            .is_err(),
        "a direct store caller cannot deliver a pre-acked message"
    );
    assert!(
        store
            .message_by_id(&pre_acked.identity.id)
            .expect("read")
            .is_none(),
        "the invalid send commits neither message nor audit"
    );
    assert!(
        store
            .audit_by_subject_kind(&pre_acked.identity.id, AuditKind::MessageSend, None, 10)
            .expect("audit history")
            .events
            .is_empty()
    );
}

#[test]
fn recipient_reader_is_ordered_keyset_paginated_and_visibility_isolated() {
    let store = store();
    let sender = id(10);
    let bob = "agent:00000000-0000-0000-0000-000000000020";
    let alice = "agent:00000000-0000-0000-0000-000000000021";
    let room = id(50);
    let other_room = id(51);

    let mut second = message(
        id(2),
        sender,
        bob,
        "2026-06-06T09:00:00Z[UTC]",
        "2026-06-06T09:00:00Z[UTC]",
        "second by id",
    );
    second.room_id = Some(room);
    let mut first = message(
        id(1),
        sender,
        bob,
        "2026-06-06T09:00:00Z[UTC]",
        "2026-06-06T09:00:00Z[UTC]",
        "first by id",
    );
    first.room_id = Some(room);
    let mut third = message(
        id(3),
        sender,
        bob,
        "2026-06-06T10:00:00Z[UTC]",
        "2026-06-06T10:00:00Z[UTC]",
        "third by time",
    );
    third.room_id = Some(room);
    let mut outside_room = message(
        id(4),
        sender,
        bob,
        "2026-06-06T11:00:00Z[UTC]",
        "2026-06-06T11:00:00Z[UTC]",
        "other room",
    );
    outside_room.room_id = Some(other_room);
    for value in [&third, &second, &outside_room, &first] {
        save(&store, value);
    }

    let page1 = store
        .messages_for_recipient(bob, Some(&room), None, 2, false)
        .expect("page one");
    assert_eq!(
        page1
            .messages
            .iter()
            .map(|message| message.identity.id)
            .collect::<Vec<_>>(),
        vec![id(1), id(2)],
        "equal ingestion instants use id as the deterministic tiebreak"
    );
    let cursor = page1.next.expect("third matching row means continuation");
    assert_eq!(cursor, MessageCursor::of(&second));

    let page2 = store
        .messages_for_recipient(bob, Some(&room), Some(&cursor), 2, false)
        .expect("page two");
    assert_eq!(page2.messages.len(), 1);
    assert_eq!(page2.messages[0].identity.id, id(3));
    assert!(page2.next.is_none(), "no continuation at the end");

    let replay = store
        .messages_for_recipient(bob, Some(&room), Some(&cursor), 2, false)
        .expect("same cursor");
    assert_eq!(replay.messages[0].identity.id, id(3));
    assert!(
        replay
            .messages
            .iter()
            .all(|message| message.identity.id != id(2)),
        "cursor is strictly exclusive"
    );

    assert!(
        store
            .messages_for_recipient(alice, None, None, 20, false)
            .expect("alice inbox")
            .messages
            .is_empty(),
        "a third agent cannot see Bob's recipient-inbox rows"
    );
}

#[test]
fn read_state_change_is_an_audited_compare_and_set() {
    let store = store();
    let sender = id(10);
    let recipient = id(20);
    let sent = message(
        id(1),
        sender,
        &format!("agent:{recipient}"),
        "2026-06-06T09:00:00Z[UTC]",
        "2026-06-06T09:00:00Z[UTC]",
        "body",
    );
    save(&store, &sent);

    let read = store
        .set_message_read_state(
            &sent.identity.id,
            MessageReadState::Read,
            Some(MessageReadState::Unread),
            &recipient,
            &ts("2026-06-06T10:00:00Z[UTC]"),
        )
        .expect("mark read");
    assert_eq!(read.read_state, MessageReadState::Read);

    assert!(
        store
            .set_message_read_state(
                &sent.identity.id,
                MessageReadState::Acked,
                Some(MessageReadState::Unread),
                &recipient,
                &ts("2026-06-06T11:00:00Z[UTC]"),
            )
            .is_err(),
        "stale expected_from is refused"
    );
    assert_eq!(
        store
            .message_by_id(&sent.identity.id)
            .expect("read")
            .expect("present")
            .read_state,
        MessageReadState::Read,
        "conflict writes no state"
    );

    assert!(
        store
            .set_message_read_state(
                &sent.identity.id,
                MessageReadState::Unread,
                None,
                &recipient,
                &ts("2026-06-06T11:30:00Z[UTC]"),
            )
            .is_err(),
        "Read cannot downgrade to Unread even without a CAS precondition"
    );

    store
        .set_message_read_state(
            &sent.identity.id,
            MessageReadState::Acked,
            Some(MessageReadState::Read),
            &recipient,
            &ts("2026-06-06T12:00:00Z[UTC]"),
        )
        .expect("ack");
    assert!(
        store
            .set_message_read_state(
                &sent.identity.id,
                MessageReadState::Read,
                Some(MessageReadState::Acked),
                &recipient,
                &ts("2026-06-06T12:30:00Z[UTC]"),
            )
            .is_err(),
        "Acked cannot downgrade to Read"
    );
    store
        .set_message_read_state(
            &sent.identity.id,
            MessageReadState::Acked,
            Some(MessageReadState::Acked),
            &recipient,
            &ts("2026-06-06T13:00:00Z[UTC]"),
        )
        .expect("same state is a no-op");

    let history = store
        .audit_by_subject_kind(
            &sent.identity.id,
            AuditKind::MessageReadStateChange,
            None,
            10,
        )
        .expect("state audit");
    assert_eq!(history.events.len(), 2, "only two applied flips audit");
    assert_eq!(
        history.events[0].payload,
        serde_json::json!({ "from": "unread", "to": "read" })
    );
    assert_eq!(
        history.events[1].payload,
        serde_json::json!({ "from": "read", "to": "acked" })
    );

    assert!(
        store
            .messages_for_recipient(&format!("agent:{recipient}"), None, None, 10, true)
            .expect("unread-only poll")
            .messages
            .is_empty(),
        "acked messages do not appear in unread-only polls"
    );
}

#[test]
fn expired_message_rejects_read_state_cas_without_an_audit() {
    let store = store();
    let sender = id(10);
    let recipient = id(20);
    let sent = message(
        id(1),
        sender,
        &format!("agent:{recipient}"),
        "2026-06-06T09:00:00Z[UTC]",
        "2026-01-01T00:00:00Z[UTC]",
        "expired body",
    );
    save(&store, &sent);
    assert_eq!(
        store
            .reap_messages(&ts("2026-06-10T00:00:00Z[UTC]"), 30, 90)
            .expect("expire old unread message")
            .unacked_expired,
        1
    );

    assert!(
        store
            .set_message_read_state(
                &sent.identity.id,
                MessageReadState::Read,
                Some(MessageReadState::Unread),
                &recipient,
                &ts("2026-06-10T01:00:00Z[UTC]"),
            )
            .is_err(),
        "expired messages reject read-state CAS"
    );
    let stored = store
        .message_by_id(&sent.identity.id)
        .expect("read")
        .expect("soft-expired row remains");
    assert_eq!(stored.read_state, MessageReadState::Unread);
    assert!(stored.identity.expired_at.is_some());
    assert!(
        store
            .audit_by_subject_kind(
                &sent.identity.id,
                AuditKind::MessageReadStateChange,
                None,
                10,
            )
            .expect("state audit history")
            .events
            .is_empty(),
        "rejected expired CAS emits no audit"
    );
}

#[test]
fn retention_soft_expires_by_ack_state_and_is_idempotent() {
    let store = store();
    let sender = id(10);
    let recipient = "agent:00000000-0000-0000-0000-000000000020";
    let acked_old = message(
        id(1),
        sender,
        recipient,
        "2026-06-09T01:00:00Z[UTC]",
        "2026-04-01T00:00:00Z[UTC]",
        "acked old",
    );
    let acked_on_cutoff = message(
        id(2),
        sender,
        recipient,
        "2026-06-09T02:00:00Z[UTC]",
        "2026-05-11T00:00:00Z[UTC]",
        "acked exact cutoff",
    );
    let unread_old = message(
        id(3),
        sender,
        recipient,
        "2026-06-09T03:00:00Z[UTC]",
        "2026-01-01T00:00:00Z[UTC]",
        "unread old",
    );
    let read_old = message(
        id(4),
        sender,
        recipient,
        "2026-06-09T04:00:00Z[UTC]",
        "2026-01-02T00:00:00Z[UTC]",
        "read old",
    );
    let unread_recent = message(
        id(5),
        sender,
        recipient,
        "2026-06-09T05:00:00Z[UTC]",
        "2026-06-01T00:00:00Z[UTC]",
        "unread recent",
    );
    for value in [
        &acked_old,
        &acked_on_cutoff,
        &unread_old,
        &read_old,
        &unread_recent,
    ] {
        save(&store, value);
    }
    let recipient_actor = id(20);
    for value in [&acked_old, &acked_on_cutoff] {
        store
            .set_message_read_state(
                &value.identity.id,
                MessageReadState::Acked,
                Some(MessageReadState::Unread),
                &recipient_actor,
                &value.identity.ingested_at,
            )
            .expect("ack retention fixture through the lifecycle API");
    }
    store
        .set_message_read_state(
            &read_old.identity.id,
            MessageReadState::Read,
            Some(MessageReadState::Unread),
            &recipient_actor,
            &read_old.identity.ingested_at,
        )
        .expect("mark retention fixture read through the lifecycle API");

    let before = store.message_counts().expect("counts before");
    assert_eq!(before.unread, 2);
    assert_eq!(before.read, 1);
    assert_eq!(before.acked, 2);
    assert_eq!(before.total(), 5);

    let now = ts("2026-06-10T00:00:00Z[UTC]");
    let report = store.reap_messages(&now, 30, 90).expect("retention sweep");
    assert_eq!(report.acked_expired, 1);
    assert_eq!(report.unacked_expired, 2);
    assert_eq!(report.total(), 3);

    let inbox = store
        .messages_for_recipient(recipient, None, None, 20, false)
        .expect("live inbox");
    assert_eq!(
        inbox
            .messages
            .iter()
            .map(|message| message.identity.id)
            .collect::<Vec<_>>(),
        vec![id(2), id(5)],
        "exact-cutoff ack and in-window unread remain live"
    );
    assert_eq!(
        store
            .message_by_id(&acked_old.identity.id)
            .expect("read expired")
            .expect("row retained")
            .identity
            .expired_at
            .as_ref(),
        Some(&now),
        "retention is a soft expiry, not a hard delete"
    );
    assert_eq!(
        store.reap_messages(&now, 30, 90).expect("repeat").total(),
        0,
        "already-expired rows never re-enter the sweep"
    );
    assert_eq!(store.message_counts().expect("live counts").total(), 2);
}

#[test]
fn message_is_absent_from_memory_and_forget_label_sets() {
    assert!(!MEMORY_LABELS.contains(&Message::LABEL));
    assert!(!FORGET_SCAN_LABELS.contains(&Message::LABEL));

    let store = store();
    let sender = id(10);
    let sent = message(
        id(1),
        sender,
        "agent:00000000-0000-0000-0000-000000000020",
        "2026-06-06T09:00:00Z[UTC]",
        "2026-06-06T09:00:00Z[UTC]",
        "same phrase a recall test may index elsewhere",
    );
    let memories_before = store.memory_counts().expect("memory census").total();
    save(&store, &sent);
    assert_eq!(
        store.memory_counts().expect("memory census").total(),
        memories_before,
        "Message never inflates the memory census"
    );
    assert_eq!(store.message_counts().expect("message census").total(), 1);
}
