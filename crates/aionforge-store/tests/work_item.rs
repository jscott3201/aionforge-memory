//! Integration tests for the work-tracking store surface (work-structure design §2–§3).
//!
//! These pin the L0 contract the work-tracking layer composes: a work item round-trips
//! byte-for-byte; the OPEN caller-defined `level` accepts any harness vocabulary (not just
//! coding levels); children read back under their parent ordered by `ordinal`; items filter
//! by `work_status`; a tag is content-addressed and idempotent per `(namespace, slug)`; and
//! the `HAS_TAG` edge's FROM-union accepts a `WorkItem -> Tag` endpoint pair.

mod common;

use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::work::{WorkItem, WorkStatus};
use aionforge_store::{BoundQuery, QueryResult, Store, Value};

use common::{entity, identity, store, ts, zdt};

const T1: &str = "2026-06-06T10:00:00-05:00[America/Chicago]";

fn ns() -> Namespace {
    Namespace::Agent("alice".to_string())
}

fn work_item(
    level: &str,
    title: &str,
    status: WorkStatus,
    parent: Option<Id>,
    ordinal: u64,
) -> WorkItem {
    WorkItem {
        identity: identity(Id::generate()),
        title: title.to_string(),
        body: None,
        level: level.to_string(),
        work_status: status,
        parent_id: parent,
        ordinal,
    }
}

#[test]
fn a_fully_populated_work_item_round_trips_by_node_id_and_domain_id() {
    let store = store();
    // Every optional present: body=Some, parent_id=Some (a dangling parent — the L0 store
    // enforces no FK, the orphan guard is a higher layer), a non-default status, nonzero
    // ordinal. Proves the full property surface round-trips byte-for-byte.
    let mut item = work_item(
        "task",
        "ship the facet",
        WorkStatus::InProgress,
        Some(Id::generate()),
        7,
    );
    item.body = Some("with the open level vocabulary".to_string());

    let node = store.save_work_item(&item).expect("save work item");

    let by_node = store
        .work_item_by_node_id(node)
        .expect("read by node")
        .expect("present");
    assert_eq!(by_node, item, "round-trips byte-for-byte by node id");

    let by_id = store
        .work_item_by_id(&item.identity.id)
        .expect("read by domain id")
        .expect("present");
    assert_eq!(by_id, item, "round-trips byte-for-byte by domain id");
}

#[test]
fn a_minimal_work_item_round_trips_exercising_the_omit_when_none_branches() {
    let store = store();
    // Every optional absent: body=None, parent_id=None, default status, ordinal 0. Exercises
    // the omit-when-None write branches and the absent-property read branches.
    let item = work_item("task", "minimal", WorkStatus::Todo, None, 0);
    assert!(item.body.is_none() && item.parent_id.is_none());

    let node = store.save_work_item(&item).expect("save");
    let read = store
        .work_item_by_node_id(node)
        .expect("read")
        .expect("present");
    assert_eq!(read, item, "minimal item round-trips byte-for-byte");
    assert!(read.body.is_none(), "absent body stays None");
    assert!(read.parent_id.is_none(), "absent parent stays None");
}

#[test]
fn the_level_is_an_open_caller_defined_vocabulary() {
    // The harness-agnostic guarantee: a non-coding level (a writing agent's `chapter`) is a
    // plain string that round-trips, with no closed enum to recompile.
    let store = store();
    let item = work_item("chapter", "draft the intro", WorkStatus::Todo, None, 0);
    let node = store.save_work_item(&item).expect("save");
    let read = store
        .work_item_by_node_id(node)
        .expect("read")
        .expect("present");
    assert_eq!(read.level, "chapter");
    assert_eq!(read.work_status, WorkStatus::Todo, "default-ish todo state");
}

#[test]
fn children_read_back_under_their_parent_ordered_by_ordinal() {
    let store = store();
    let parent = work_item("milestone", "M8", WorkStatus::InProgress, None, 0);
    store.save_work_item(&parent).expect("save parent");
    let parent_id = parent.identity.id;

    // Insert out of order (2, 0, 1); they must come back 0, 1, 2.
    for ordinal in [2u64, 0, 1] {
        let child = work_item(
            "task",
            &format!("task {ordinal}"),
            WorkStatus::Todo,
            Some(parent_id),
            ordinal,
        );
        store.save_work_item(&child).expect("save child");
    }

    let children = store
        .work_items_by_parent(&parent_id)
        .expect("children of parent");
    let ordinals: Vec<u64> = children.iter().map(|c| c.ordinal).collect();
    assert_eq!(ordinals, vec![0, 1, 2], "siblings ordered by ordinal");
    assert_eq!(children.len(), 3, "exactly the three children");
    assert!(
        children.iter().all(|c| c.parent_id == Some(parent_id)),
        "every child points at the parent",
    );

    // A parent that no item references as its parent has no children — a real empty probe
    // (the prior root item has no parent_id property, so it is unreachable by any parent probe).
    let unreferenced = Id::generate();
    assert!(
        store
            .work_items_by_parent(&unreferenced)
            .expect("probe")
            .is_empty(),
        "an id no item points at yields no children",
    );
}

#[test]
fn children_do_not_bleed_across_distinct_parents() {
    let store = store();
    let parent_a = work_item("milestone", "A", WorkStatus::InProgress, None, 0);
    let parent_b = work_item("milestone", "B", WorkStatus::InProgress, None, 0);
    store.save_work_item(&parent_a).expect("save A");
    store.save_work_item(&parent_b).expect("save B");

    let mut a_ids = Vec::new();
    for ordinal in 0..3u64 {
        let child = work_item(
            "task",
            "a",
            WorkStatus::Todo,
            Some(parent_a.identity.id),
            ordinal,
        );
        a_ids.push(child.identity.id);
        store.save_work_item(&child).expect("save A child");
    }
    let mut b_ids = Vec::new();
    for ordinal in 0..2u64 {
        let child = work_item(
            "task",
            "b",
            WorkStatus::Todo,
            Some(parent_b.identity.id),
            ordinal,
        );
        b_ids.push(child.identity.id);
        store.save_work_item(&child).expect("save B child");
    }

    let got_a: std::collections::BTreeSet<Id> = store
        .work_items_by_parent(&parent_a.identity.id)
        .expect("A children")
        .iter()
        .map(|c| c.identity.id)
        .collect();
    let got_b: std::collections::BTreeSet<Id> = store
        .work_items_by_parent(&parent_b.identity.id)
        .expect("B children")
        .iter()
        .map(|c| c.identity.id)
        .collect();
    assert_eq!(
        got_a,
        a_ids.into_iter().collect(),
        "A sees only A's children"
    );
    assert_eq!(
        got_b,
        b_ids.into_iter().collect(),
        "B sees only B's children"
    );
}

#[test]
fn items_filter_by_work_status() {
    let store = store();
    let a = work_item("task", "a", WorkStatus::InProgress, None, 0);
    let b = work_item("task", "b", WorkStatus::InProgress, None, 1);
    let c = work_item("task", "c", WorkStatus::Done, None, 2);
    store.save_work_item(&a).expect("save a");
    store.save_work_item(&b).expect("save b");
    store.save_work_item(&c).expect("save c");

    // Assert the exact id set, not just a count — a count-and-predicate check cannot tell the
    // correct {a, b} from some other 2-element in-progress set.
    let in_progress: std::collections::BTreeSet<Id> = store
        .work_items_by_status(WorkStatus::InProgress)
        .expect("in-progress items")
        .iter()
        .map(|i| i.identity.id)
        .collect();
    assert_eq!(
        in_progress,
        [a.identity.id, b.identity.id].into_iter().collect(),
        "exactly a and b are in progress",
    );

    let done = store
        .work_items_by_status(WorkStatus::Done)
        .expect("done items");
    assert_eq!(done.len(), 1, "one done item");
    assert_eq!(done[0].identity.id, c.identity.id);

    assert!(
        store
            .work_items_by_status(WorkStatus::Blocked)
            .expect("blocked items")
            .is_empty(),
        "no blocked items",
    );
}

#[test]
fn ensure_tag_is_idempotent_per_namespace_and_slug() {
    let store = store();
    let (id_a, node_a) = store
        .ensure_tag(&ns(), "pr6", Some("PR 6"), &ts(T1))
        .expect("ensure first");
    let (id_b, node_b) = store
        .ensure_tag(&ns(), "pr6", None, &ts(T1))
        .expect("ensure again");

    assert_eq!(
        id_a, id_b,
        "same (namespace, slug) -> same content-addressed id"
    );
    assert_eq!(node_a, node_b, "and the same node — no duplicate minted");

    // A different slug is a different tag.
    let (id_c, _) = store
        .ensure_tag(&ns(), "tech-debt", None, &ts(T1))
        .expect("ensure other");
    assert_ne!(id_a, id_c, "a different slug is a different tag");

    // The first call's display is retained; the read resolves by (namespace, slug).
    let read = store
        .tag_by_slug(&ns(), "pr6")
        .expect("read tag")
        .expect("present");
    assert_eq!(read.slug, "pr6");
    assert_eq!(read.display.as_deref(), Some("PR 6"));
    assert_eq!(read.identity.id, id_a);
}

#[test]
fn has_tag_accepts_a_work_item_to_tag_endpoint_pair() {
    // The catalog FROM-union (WorkItem -> Tag) is a one-way door; prove the closed graph
    // accepts the endpoints. The typed tag-wiring tool lands in PR3; here we wire the edge
    // through the bound write path to validate the schema, not the surface.
    let store = store();
    let item = work_item("task", "tag me", WorkStatus::Todo, None, 0);
    store.save_work_item(&item).expect("save item");
    let (tag_id, _) = store
        .ensure_tag(&ns(), "auth", None, &ts(T1))
        .expect("ensure tag");

    let query = BoundQuery::new(
        "MATCH (w:WorkItem {id: $from}), (t:Tag {id: $to}) INSERT (w)-[:HAS_TAG]->(t)",
    )
    .bind_uuid("from", item.identity.id)
    .expect("bind from")
    .bind_uuid("to", tag_id)
    .expect("bind to");
    store
        .execute(&query)
        .expect("HAS_TAG accepts WorkItem -> Tag");
}

#[test]
fn tags_are_isolated_by_namespace() {
    // The content-addressed id mixes the namespace in, so the same slug under two namespaces is
    // two distinct tags — a team tag never collides with a private one.
    let store = store();
    let alice = Namespace::Agent("alice".to_string());
    let team = Namespace::Team("aionforge".to_string());

    let (id_private, node_private) = store
        .ensure_tag(&alice, "pr6", None, &ts(T1))
        .expect("private tag");
    let (id_team, node_team) = store
        .ensure_tag(&team, "pr6", None, &ts(T1))
        .expect("team tag");

    assert_ne!(
        id_private, id_team,
        "same slug, different namespace -> distinct ids"
    );
    assert_ne!(node_private, node_team, "and distinct nodes");

    // Each namespace resolves its own tag.
    assert_eq!(
        store
            .tag_by_slug(&alice, "pr6")
            .expect("read")
            .unwrap()
            .identity
            .id,
        id_private,
    );
    assert_eq!(
        store
            .tag_by_slug(&team, "pr6")
            .expect("read")
            .unwrap()
            .identity
            .id,
        id_team,
    );
}

#[test]
fn a_tag_with_no_display_round_trips_as_none() {
    let store = store();
    let (_, node) = store
        .ensure_tag(&ns(), "no-display", None, &ts(T1))
        .expect("ensure tag");
    let read = store
        .tag_by_node_id(node)
        .expect("read by node")
        .expect("present");
    assert_eq!(read.slug, "no-display");
    assert!(read.display.is_none(), "absent display stays None");
}

#[test]
fn the_immutable_db_default_work_status_and_ordinal_decode_correctly() {
    // The catalog DDL `work_status :: STRING(32) DEFAULT 'todo'` and `ordinal :: UINT DEFAULT 0`
    // are one-way doors. save_work_item always writes them explicitly, so this inserts a row via
    // the bound write path OMITTING both columns, letting the immutable DEFAULTs apply, and proves
    // they decode back to WorkStatus::Todo / 0 — pinning the DDL-literal-to-enum coupling.
    let store = store();
    let id = Id::generate();
    let insert = BoundQuery::new(
        "INSERT (w:WorkItem {id: $id, ingested_at: $ts, namespace: $ns, title: $title, level: $level})",
    )
    .bind_uuid("id", id)
    .expect("bind id")
    .bind("ts", zdt())
    .expect("bind ts")
    .bind_str("ns", "agent:alice")
    .expect("bind ns")
    .bind_str("title", "defaulted")
    .expect("bind title")
    .bind_str("level", "task")
    .expect("bind level");
    store
        .execute(&insert)
        .expect("insert work item without status/ordinal");

    let read = store.work_item_by_id(&id).expect("read").expect("present");
    assert_eq!(
        read.work_status,
        WorkStatus::Todo,
        "DB DEFAULT 'todo' decodes to Todo"
    );
    assert_eq!(read.ordinal, 0, "DB DEFAULT 0 decodes to 0");
    assert!(read.body.is_none() && read.parent_id.is_none());
}

#[test]
fn has_tag_accepts_a_retrofit_endpoint_from_an_existing_memory_kind() {
    // The HAS_TAG FROM-union spans every retrievable kind, not just WorkItem — that is the whole
    // reason the full union is declared in the one-way-door catalog. Prove a representative
    // memory kind (Entity) is accepted as a HAS_TAG source.
    let store = store();
    let e = entity("a-tagged-entity");
    store.insert_entity(&e).expect("insert entity");
    let (tag_id, _) = store
        .ensure_tag(&ns(), "topic", None, &ts(T1))
        .expect("ensure tag");

    let query = BoundQuery::new(
        "MATCH (n:Entity {id: $from}), (t:Tag {id: $to}) INSERT (n)-[:HAS_TAG]->(t)",
    )
    .bind_uuid("from", e.identity.id)
    .expect("bind from")
    .bind_uuid("to", tag_id)
    .expect("bind to");
    store
        .execute(&query)
        .expect("HAS_TAG accepts Entity -> Tag");
}

#[test]
fn reading_a_wrong_kind_node_as_a_work_item_fails() {
    let store = store();
    let (_, tag_node) = store
        .ensure_tag(&ns(), "not-a-work-item", None, &ts(T1))
        .expect("ensure tag");
    // A Tag node is missing the work-item required fields; decoding it as one fails closed.
    assert!(store.work_item_by_node_id(tag_node).is_err());
}

#[test]
fn set_parent_attaches_clears_and_is_idempotent() {
    let store = store();
    let root = work_item("milestone", "M", WorkStatus::InProgress, None, 0);
    let child = work_item("task", "c", WorkStatus::Todo, None, 0);
    store.save_work_item(&root).expect("save root");
    store.save_work_item(&child).expect("save child");

    // Attach: the child becomes reachable under the root.
    let moved = store
        .set_parent(&child.identity.id, Some(&root.identity.id))
        .expect("attach parent");
    assert_eq!(
        moved.parent_id,
        Some(root.identity.id),
        "returns the moved item"
    );
    let kids: Vec<Id> = store
        .work_items_by_parent(&root.identity.id)
        .expect("kids")
        .iter()
        .map(|k| k.identity.id)
        .collect();
    assert_eq!(
        kids,
        vec![child.identity.id],
        "child now reachable under root"
    );

    // Idempotent: re-attaching to the same parent returns unchanged and writes nothing.
    let again = store
        .set_parent(&child.identity.id, Some(&root.identity.id))
        .expect("idempotent attach");
    assert_eq!(again.parent_id, Some(root.identity.id));

    // Clear to a root: the parent_id property is REMOVED (absence, not null), so the parent probe
    // no longer finds it and a fresh read agrees.
    let cleared = store
        .set_parent(&child.identity.id, None)
        .expect("clear parent");
    assert!(cleared.parent_id.is_none(), "cleared to a root");
    assert!(
        store
            .work_items_by_parent(&root.identity.id)
            .expect("kids after clear")
            .is_empty(),
        "no longer reachable under the old parent",
    );
    let read = store
        .work_item_by_id(&child.identity.id)
        .expect("read")
        .expect("present");
    assert!(
        read.parent_id.is_none(),
        "stored item has no parent_id property"
    );
}

#[test]
fn set_parent_on_an_unknown_item_errors() {
    let store = store();
    assert!(store.set_parent(&Id::generate(), None).is_err());
}

#[test]
fn reorder_moves_siblings_and_is_idempotent() {
    let store = store();
    let parent = work_item("milestone", "M", WorkStatus::InProgress, None, 0);
    store.save_work_item(&parent).expect("save parent");
    let pid = parent.identity.id;
    let a = work_item("task", "a", WorkStatus::Todo, Some(pid), 0);
    let b = work_item("task", "b", WorkStatus::Todo, Some(pid), 1);
    store.save_work_item(&a).expect("save a");
    store.save_work_item(&b).expect("save b");

    // Push a behind b (a: 0 -> 5), so siblings come back b, a.
    let moved = store.reorder(&a.identity.id, 5).expect("reorder a");
    assert_eq!(moved.ordinal, 5);
    let order: Vec<Id> = store
        .work_items_by_parent(&pid)
        .expect("kids")
        .iter()
        .map(|k| k.identity.id)
        .collect();
    assert_eq!(order, vec![b.identity.id, a.identity.id], "b now leads a");

    // Idempotent: the same ordinal returns unchanged and writes nothing.
    let same = store
        .reorder(&a.identity.id, 5)
        .expect("idempotent reorder");
    assert_eq!(same.ordinal, 5);
}

#[test]
fn attach_tag_mints_then_dedups_the_edge() {
    let store = store();
    let item = work_item("task", "tag me", WorkStatus::Todo, None, 0);
    store.save_work_item(&item).expect("save item");

    let tag_id = store
        .attach_tag(
            WorkItem::LABEL,
            &item.identity.id,
            &ns(),
            "auth",
            Some("Auth"),
            &ts(T1),
        )
        .expect("first attach");
    // A second attach on the same (item, slug) is a no-op on both axes.
    let tag_id_again = store
        .attach_tag(
            WorkItem::LABEL,
            &item.identity.id,
            &ns(),
            "auth",
            None,
            &ts(T1),
        )
        .expect("second attach");
    assert_eq!(tag_id, tag_id_again, "same content-addressed tag");

    // Exactly one HAS_TAG edge and one Tag node despite two attaches.
    assert_eq!(
        count(
            &store,
            "MATCH (:WorkItem)-[r:HAS_TAG]->(:Tag) RETURN count(r) AS n"
        ),
        1,
        "the edge is written once and deduped on replay",
    );
    assert_eq!(
        count(&store, "MATCH (t:Tag) RETURN count(t) AS n"),
        1,
        "one tag node"
    );

    // The tag was minted in the item's namespace and resolves by slug.
    let tag = store
        .tag_by_slug(&ns(), "auth")
        .expect("read")
        .expect("present");
    assert_eq!(tag.identity.id, tag_id);
}

#[test]
fn attach_tag_to_a_missing_source_errors_and_mints_no_tag() {
    let store = store();
    assert!(
        store
            .attach_tag(
                WorkItem::LABEL,
                &Id::generate(),
                &ns(),
                "ghost",
                None,
                &ts(T1)
            )
            .is_err(),
        "a missing source is rejected",
    );
    assert!(
        store.tag_by_slug(&ns(), "ghost").expect("read").is_none(),
        "the source is resolved before the tag is minted, so no orphan tag is left behind",
    );
}

#[test]
fn work_counts_group_by_status_and_never_touch_the_memory_census() {
    let store = store();
    let memories_before = store.memory_counts().expect("memory counts").total();

    for (title, status) in [
        ("a", WorkStatus::Todo),
        ("b", WorkStatus::Todo),
        ("c", WorkStatus::InProgress),
        ("d", WorkStatus::Done),
    ] {
        store
            .save_work_item(&work_item("task", title, status, None, 0))
            .expect("save");
    }
    // A tag is also exempt from the memory census.
    store.ensure_tag(&ns(), "x", None, &ts(T1)).expect("tag");
    // A real memory (entity) is counted as a memory, never as work — the cross-check.
    store
        .insert_entity(&entity("real-memory"))
        .expect("insert entity");

    let wc = store.work_counts().expect("work counts");
    assert_eq!(wc.todo, 2);
    assert_eq!(wc.in_progress, 1);
    assert_eq!(wc.done, 1);
    assert_eq!(wc.blocked, 0);
    assert_eq!(wc.dropped, 0);
    assert_eq!(
        wc.total(),
        4,
        "the five buckets sum to exactly the four work items"
    );

    assert_eq!(
        store.memory_counts().expect("memory counts").total(),
        memories_before + 1,
        "the entity is a memory; the four work items and the tag are not",
    );
}

/// Scalar count of a `RETURN count(...) AS n` probe (mirrors the work_status.rs helper).
fn count(store: &Store, pattern: &str) -> u64 {
    match store
        .execute(&BoundQuery::new(pattern))
        .expect("count query")
    {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Uint(n)) => *n,
            Some(Value::Int(n)) => u64::try_from(*n).unwrap_or(0),
            _ => 0,
        },
        _ => 0,
    }
}
