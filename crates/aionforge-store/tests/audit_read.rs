//! Acceptance tests for the L0 audit read surface (06 §6, M4.T06).
//!
//! Pins the by-subject full history, by-kind scoping, by-(subject, kind), the `(occurred_at, id)`
//! ordering (by absolute instant, not lexical string), and the keyset-cursor pagination contract.

use aionforge_domain::blocks::Identity;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_store::{AuditCursor, AuditHistory, Store};

use jiff::Zoned;

fn ts(s: &str) -> Zoned {
    s.parse().expect("valid zoned datetime")
}

fn store() -> Store {
    Store::open_in_memory_migrated(&ts("2026-06-06T12:00:00-05:00[America/Chicago]")).expect("open")
}

fn id(marker: &str) -> Id {
    Id::from_content_hash(marker.as_bytes())
}

/// An audit event keyed by `marker` (its id), about `subject`, of `kind`, at `occurred`.
fn audit(marker: &str, subject: Id, kind: AuditKind, occurred: &str) -> AuditEvent {
    AuditEvent {
        identity: Identity {
            id: id(marker),
            ingested_at: ts("2026-06-06T12:00:00-05:00[America/Chicago]"),
            namespace: Namespace::System,
            expired_at: None,
        },
        kind,
        subject_id: subject,
        actor_id: id("substrate"),
        payload: serde_json::json!({ "marker": marker }),
        signature: String::new(),
        occurred_at: ts(occurred),
    }
}

/// The event ids of a page, in order.
fn ids(page: &AuditHistory) -> Vec<Id> {
    page.events.iter().map(|e| e.identity.id).collect()
}

#[test]
fn audit_history_returns_all_kinds_oldest_first_scoped_to_subject() {
    let store = store();
    let subj = id("subject");
    // Distinct kinds, committed out of chronological order.
    store
        .commit_audit(&audit(
            "e3",
            subj,
            AuditKind::Demote,
            "2026-06-06T12:00:00+00:00[UTC]",
        ))
        .unwrap();
    store
        .commit_audit(&audit(
            "e1",
            subj,
            AuditKind::Promote,
            "2026-06-06T10:00:00+00:00[UTC]",
        ))
        .unwrap();
    store
        .commit_audit(&audit(
            "e2",
            subj,
            AuditKind::Attest,
            "2026-06-06T11:00:00+00:00[UTC]",
        ))
        .unwrap();
    // A different subject's event must not leak into this subject's history.
    store
        .commit_audit(&audit(
            "other",
            id("elsewhere"),
            AuditKind::Promote,
            "2026-06-06T09:00:00+00:00[UTC]",
        ))
        .unwrap();

    let page = store.audit_history(&subj, None, 50).unwrap();
    assert_eq!(
        ids(&page),
        vec![id("e1"), id("e2"), id("e3")],
        "all kinds, oldest first, scoped to the subject"
    );
    assert!(page.next.is_none(), "the full history fits in one page");
    assert_eq!(store.audit_count_for_subject(&subj).unwrap(), 3);
}

#[test]
fn audit_history_orders_by_instant_not_lexical_string() {
    let store = store();
    let subj = id("subject");
    // tz lands at 13:30 UTC — chronologically between e-early (13:00 UTC) and e-late (14:00 UTC) —
    // but its local string ("...T08:30...-05:00") sorts BELOW both. A string sort would misplace it.
    store
        .commit_audit(&audit(
            "e-late",
            subj,
            AuditKind::Promote,
            "2026-06-06T14:00:00+00:00[UTC]",
        ))
        .unwrap();
    store
        .commit_audit(&audit(
            "e-early",
            subj,
            AuditKind::Promote,
            "2026-06-06T13:00:00+00:00[UTC]",
        ))
        .unwrap();
    store
        .commit_audit(&audit(
            "e-tz",
            subj,
            AuditKind::Promote,
            "2026-06-06T08:30:00-05:00[America/Chicago]",
        ))
        .unwrap();

    let page = store.audit_history(&subj, None, 50).unwrap();
    assert_eq!(
        ids(&page),
        vec![id("e-early"), id("e-tz"), id("e-late")],
        "ordered by absolute instant, so the tz-twisted event sorts in the middle"
    );
}

#[test]
fn audit_by_kind_returns_only_that_kind_across_subjects() {
    let store = store();
    store
        .commit_audit(&audit(
            "p1",
            id("subj-a"),
            AuditKind::Promote,
            "2026-06-06T10:00:00+00:00[UTC]",
        ))
        .unwrap();
    store
        .commit_audit(&audit(
            "p2",
            id("subj-b"),
            AuditKind::Promote,
            "2026-06-06T11:00:00+00:00[UTC]",
        ))
        .unwrap();
    store
        .commit_audit(&audit(
            "d1",
            id("subj-a"),
            AuditKind::Demote,
            "2026-06-06T12:00:00+00:00[UTC]",
        ))
        .unwrap();

    let page = store.audit_by_kind(AuditKind::Promote, None, 50).unwrap();
    assert_eq!(
        ids(&page),
        vec![id("p1"), id("p2")],
        "only Promote events, across subjects, oldest first"
    );
}

#[test]
fn audit_by_subject_kind_narrows_to_one_kind() {
    let store = store();
    let subj = id("subject");
    store
        .commit_audit(&audit(
            "p",
            subj,
            AuditKind::Promote,
            "2026-06-06T10:00:00+00:00[UTC]",
        ))
        .unwrap();
    store
        .commit_audit(&audit(
            "d",
            subj,
            AuditKind::Demote,
            "2026-06-06T11:00:00+00:00[UTC]",
        ))
        .unwrap();

    let page = store
        .audit_by_subject_kind(&subj, AuditKind::Demote, None, 50)
        .unwrap();
    assert_eq!(
        ids(&page),
        vec![id("d")],
        "only the subject's Demote events"
    );
}

#[test]
fn audit_history_paginates_by_keyset_cursor_without_gaps_or_overlap() {
    let store = store();
    let subj = id("subject");
    for (i, marker) in ["e1", "e2", "e3", "e4", "e5"].iter().enumerate() {
        let occurred = format!("2026-06-06T1{i}:00:00+00:00[UTC]");
        store
            .commit_audit(&audit(marker, subj, AuditKind::Promote, &occurred))
            .unwrap();
    }

    // Page 1.
    let p1 = store.audit_history(&subj, None, 2).unwrap();
    assert_eq!(ids(&p1), vec![id("e1"), id("e2")]);
    let c1 = p1.next.expect("more pages remain");

    // A late append after the cursor is picked up; an early append before it never shifts the page.
    store
        .commit_audit(&audit(
            "e0",
            subj,
            AuditKind::Promote,
            "2026-06-06T08:00:00+00:00[UTC]",
        ))
        .unwrap();

    // Page 2 — strictly after the cursor, so e0 (before it) is excluded and there is no overlap.
    let p2 = store.audit_history(&subj, Some(&c1), 2).unwrap();
    assert_eq!(ids(&p2), vec![id("e3"), id("e4")]);
    let c2 = p2.next.expect("one more page");

    // Page 3 — the last event, no continuation.
    let p3 = store.audit_history(&subj, Some(&c2), 2).unwrap();
    assert_eq!(ids(&p3), vec![id("e5")]);
    assert!(p3.next.is_none(), "history exhausted");
}

#[test]
fn an_empty_subject_yields_an_empty_page() {
    let store = store();
    let page = store.audit_history(&id("nobody"), None, 10).unwrap();
    assert!(page.events.is_empty() && page.next.is_none());
    assert_eq!(store.audit_count_for_subject(&id("nobody")).unwrap(), 0);
}

#[test]
fn a_zero_limit_is_clamped_to_one_event() {
    let store = store();
    let subj = id("subject");
    store
        .commit_audit(&audit(
            "a",
            subj,
            AuditKind::Promote,
            "2026-06-06T10:00:00+00:00[UTC]",
        ))
        .unwrap();
    store
        .commit_audit(&audit(
            "b",
            subj,
            AuditKind::Promote,
            "2026-06-06T11:00:00+00:00[UTC]",
        ))
        .unwrap();

    let page = store.audit_history(&subj, None, 0).unwrap();
    assert_eq!(ids(&page), vec![id("a")], "limit 0 is raised to 1");
    assert_eq!(
        page.next,
        Some(AuditCursor {
            occurred_at: ts("2026-06-06T10:00:00+00:00[UTC]"),
            id: id("a"),
        }),
        "the continuation cursor points at the single returned event"
    );
}
