//! Integration tests for the work-item status transition and the work-tracking exemption
//! (work-structure design §2; PR2).
//!
//! These pin: an advance is a guarded compare-and-set that flips `work_status` and co-commits a
//! signed `WorkStatusChange` audit anchored on the work item; a stale precondition is refused with
//! nothing written; the lifecycle is recoverable as the by-subject audit history; and the
//! work-tracking kinds stay out of the memory census and the public forget scan set (the
//! exemption-by-omission regression lock — the forget/pin/erase behavioral proofs live in the
//! aionforge-forget crate tests).

mod common;

use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::forensic::AuditKind;
use aionforge_domain::nodes::work::{Tag, WorkItem, WorkStatus};
use aionforge_store::{BoundQuery, FORGET_SCAN_LABELS, MEMORY_LABELS, QueryResult, Store, Value};

use common::{identity, store, ts};

const T1: &str = "2026-06-06T10:00:00-05:00[America/Chicago]";
const T2: &str = "2026-06-06T11:00:00-05:00[America/Chicago]";

fn work_item(status: WorkStatus) -> WorkItem {
    WorkItem {
        identity: identity(Id::generate()),
        title: "ship it".to_string(),
        body: None,
        level: "task".to_string(),
        work_status: status,
        parent_id: None,
        ordinal: 0,
    }
}

#[test]
fn advance_flips_status_and_records_a_signed_transition() {
    let store = store();
    let item = work_item(WorkStatus::Todo);
    store.save_work_item(&item).expect("save");
    let actor = Id::generate();

    let updated = store
        .advance_work_status(
            &item.identity.id,
            WorkStatus::InProgress,
            Some(WorkStatus::Todo),
            &actor,
            &ts(T1),
        )
        .expect("advance");
    assert_eq!(
        updated.work_status,
        WorkStatus::InProgress,
        "returns the moved item"
    );

    // The flip is persisted to one node (no version node minted).
    let read = store
        .work_item_by_id(&item.identity.id)
        .expect("read")
        .expect("present");
    assert_eq!(read.work_status, WorkStatus::InProgress);
    assert_eq!(
        read.identity.id, item.identity.id,
        "still one node for life"
    );

    // The transition is the by-subject audit trail as a WorkStatusChange, signed.
    let history = store
        .audit_by_subject_kind(&item.identity.id, AuditKind::WorkStatusChange, None, 50)
        .expect("audit history");
    assert_eq!(history.events.len(), 1, "one transition recorded");
    let event = &history.events[0];
    assert_eq!(event.subject_id, item.identity.id);
    assert_eq!(event.actor_id, actor);
    assert_eq!(
        event.payload,
        serde_json::json!({ "from": "todo", "to": "in_progress" })
    );
}

#[test]
fn advance_refuses_a_stale_compare_and_set_and_writes_nothing() {
    let store = store();
    let item = work_item(WorkStatus::Todo);
    store.save_work_item(&item).expect("save");

    // The item is Todo; advancing with expected_from=Done is a stale CAS — refused.
    let outcome = store.advance_work_status(
        &item.identity.id,
        WorkStatus::InProgress,
        Some(WorkStatus::Done),
        &Id::generate(),
        &ts(T1),
    );
    assert!(outcome.is_err(), "a stale expected_from is refused");

    // Nothing changed and no audit was written (the CAS guard precedes every mutation).
    let read = store
        .work_item_by_id(&item.identity.id)
        .expect("read")
        .expect("present");
    assert_eq!(
        read.work_status,
        WorkStatus::Todo,
        "status unchanged after a refusal"
    );
    assert_eq!(
        store
            .audit_count_for_subject(&item.identity.id)
            .expect("count"),
        0,
        "a refused transition leaves no audit",
    );
}

#[test]
fn advancing_to_the_current_status_is_a_no_op_that_writes_nothing() {
    let store = store();
    let item = work_item(WorkStatus::InProgress);
    store.save_work_item(&item).expect("save");

    // A transition to the status the item already holds is a state-gated no-op — both with a
    // matching precondition and with none. It returns the unchanged item and builds no audit,
    // so the by-subject history never carries a phantom `{from: X, to: X}` row.
    let returned = store
        .advance_work_status(
            &item.identity.id,
            WorkStatus::InProgress,
            Some(WorkStatus::InProgress),
            &Id::generate(),
            &ts(T1),
        )
        .expect("a guarded no-op advance is Ok");
    assert_eq!(
        returned.work_status,
        WorkStatus::InProgress,
        "returns unchanged"
    );
    store
        .advance_work_status(
            &item.identity.id,
            WorkStatus::InProgress,
            None,
            &Id::generate(),
            &ts(T2),
        )
        .expect("an unconditional no-op advance is Ok");

    let read = store
        .work_item_by_id(&item.identity.id)
        .expect("read")
        .expect("present");
    assert_eq!(read.work_status, WorkStatus::InProgress, "status untouched");
    assert_eq!(
        store
            .audit_count_for_subject(&item.identity.id)
            .expect("count"),
        0,
        "a no-op transition records no audit",
    );
    assert_eq!(
        audit_edges(&store),
        0,
        "a no-op transition wires no AUDIT edge"
    );
}

#[test]
fn each_applied_transition_writes_exactly_one_audit_node_and_edge() {
    let store = store();
    let item = work_item(WorkStatus::Todo);
    store.save_work_item(&item).expect("save");

    store
        .advance_work_status(
            &item.identity.id,
            WorkStatus::InProgress,
            Some(WorkStatus::Todo),
            &Id::generate(),
            &ts(T1),
        )
        .expect("advance");

    // One applied transition is one audit node anchored by exactly one AUDIT edge — the
    // regression lock against a duplicate edge leaking past the `created` guard.
    assert_eq!(
        store
            .audit_count_for_subject(&item.identity.id)
            .expect("count"),
        1,
        "one transition, one audit node",
    );
    assert_eq!(audit_edges(&store), 1, "one transition, one AUDIT edge");
}

#[test]
fn re_crossing_a_transition_at_the_same_instant_is_its_own_audit_row() {
    // The defect a content-addressed `(subject, from, to, at)` id reintroduces: a subject that
    // crosses the same transition twice at one instant must yield two distinct audit rows, never
    // one silently-deduped row whose tail contradicts the node's state. The id is generated per
    // applied flip precisely so this never collapses.
    let store = store();
    let item = work_item(WorkStatus::Todo);
    store.save_work_item(&item).expect("save");
    let actor = Id::generate();

    store
        .advance_work_status(
            &item.identity.id,
            WorkStatus::InProgress,
            Some(WorkStatus::Todo),
            &actor,
            &ts(T1),
        )
        .expect("todo -> in_progress");
    store
        .advance_work_status(
            &item.identity.id,
            WorkStatus::Todo,
            Some(WorkStatus::InProgress),
            &actor,
            &ts(T1),
        )
        .expect("in_progress -> todo at the same instant");
    store
        .advance_work_status(
            &item.identity.id,
            WorkStatus::InProgress,
            Some(WorkStatus::Todo),
            &actor,
            &ts(T1),
        )
        .expect("todo -> in_progress again at the same instant");

    let history = store
        .audit_by_subject_kind(&item.identity.id, AuditKind::WorkStatusChange, None, 50)
        .expect("history");
    assert_eq!(history.events.len(), 3, "three crossings are three rows");
    assert_eq!(audit_edges(&store), 3, "three rows, three AUDIT edges");
}

#[test]
fn advance_without_a_precondition_is_unconditional() {
    let store = store();
    let item = work_item(WorkStatus::InProgress);
    store.save_work_item(&item).expect("save");
    let updated = store
        .advance_work_status(
            &item.identity.id,
            WorkStatus::Done,
            None,
            &Id::generate(),
            &ts(T1),
        )
        .expect("advance");
    assert_eq!(updated.work_status, WorkStatus::Done);
}

#[test]
fn advancing_an_unknown_work_item_errors() {
    let store = store();
    assert!(
        store
            .advance_work_status(
                &Id::generate(),
                WorkStatus::Done,
                None,
                &Id::generate(),
                &ts(T1)
            )
            .is_err(),
        "no work item carries the id",
    );
}

#[test]
fn a_sequence_of_transitions_is_the_full_lifecycle_history() {
    let store = store();
    let item = work_item(WorkStatus::Todo);
    store.save_work_item(&item).expect("save");
    let actor = Id::generate();

    store
        .advance_work_status(
            &item.identity.id,
            WorkStatus::InProgress,
            Some(WorkStatus::Todo),
            &actor,
            &ts(T1),
        )
        .expect("todo -> in_progress");
    store
        .advance_work_status(
            &item.identity.id,
            WorkStatus::Done,
            Some(WorkStatus::InProgress),
            &actor,
            &ts(T2),
        )
        .expect("in_progress -> done");

    let history = store
        .audit_by_subject_kind(&item.identity.id, AuditKind::WorkStatusChange, None, 50)
        .expect("history");
    assert_eq!(
        history.events.len(),
        2,
        "both transitions recorded, oldest first"
    );
    assert_eq!(
        history.events[0].payload,
        serde_json::json!({ "from": "todo", "to": "in_progress" })
    );
    assert_eq!(
        history.events[1].payload,
        serde_json::json!({ "from": "in_progress", "to": "done" })
    );
}

#[test]
fn work_items_and_tags_are_excluded_from_the_memory_census() {
    let store = store();
    let before = store.memory_counts().expect("counts").total();
    store
        .save_work_item(&work_item(WorkStatus::Todo))
        .expect("save work item");
    store
        .ensure_tag(&Namespace::Agent("alice".to_string()), "pr6", None, &ts(T1))
        .expect("ensure tag");
    let after = store.memory_counts().expect("counts");
    assert_eq!(
        after.total(),
        before,
        "a work item and a tag add nothing to the memory census",
    );
}

#[test]
fn the_work_tracking_kinds_are_absent_from_the_public_forget_and_census_label_sets() {
    // The exemption-by-omission regression lock for the two label sets the store re-exports.
    // (Absence from the forgetter's ALL_MEMORY_LABELS / POINT_LABELS is proven behaviorally in the
    // aionforge-forget crate — forget/pin/erase resolve a work item to NotFound — and directly by
    // the const-membership unit test in that crate.) Absence from MEMORY_LABELS also locks the
    // "never consolidated" property transitively: consolidation discovery probes Episode (a
    // member of MEMORY_LABELS) only, so a kind absent from MEMORY_LABELS is never a candidate.
    for label in [WorkItem::LABEL, Tag::LABEL] {
        assert!(
            !FORGET_SCAN_LABELS.contains(&label),
            "{label} must not be swept by active forgetting",
        );
        assert!(
            !MEMORY_LABELS.contains(&label),
            "{label} must not be counted as a memory or discovered for consolidation",
        );
    }
}

/// Count of `AuditEvent -AUDIT-> WorkItem` edges across the store — the edge-cardinality probe
/// that proves the status-change audit anchors exactly one edge per applied transition and none
/// for a no-op (mirrors the `capture_write.rs` AUDIT-edge count helper).
fn audit_edges(store: &Store) -> u64 {
    match store
        .execute(&BoundQuery::new(
            "MATCH (:AuditEvent)-[r:AUDIT]->(:WorkItem) RETURN count(r) AS n",
        ))
        .expect("count AUDIT edges")
    {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Uint(n)) => *n,
            Some(Value::Int(n)) => u64::try_from(*n).unwrap_or(0),
            _ => 0,
        },
        _ => 0,
    }
}
