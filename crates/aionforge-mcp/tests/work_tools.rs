//! End-to-end tests for the work-tracking MCP tool cluster (work-structure design §2–§4).
//!
//! Drive the five tools through their public tool-logic entry points over a hermetic
//! in-memory [`Memory`]: the create→advance→link→tree→query round trip; the guarded
//! compare-and-set conflict; the namespace/authorization guards (unauthorized team target,
//! missing/cross-namespace parent, cross-tenant invisibility); and the read-only write-guard
//! on the three mutating tools.

use aionforge_domain::ids::Id;
use aionforge_engine::Principal;
use aionforge_mcp::{
    AuthEnabled, TokenClass, ValidatedPrincipal, WorkAdvanceToolParams, WorkCreateToolParams,
    WorkLinkToolParams, WorkQueryToolParams, WorkTreeToolParams, WritePosture, work_advance_tool,
    work_create_tool, work_link_tool, work_query_tool, work_tree_tool,
};

mod common;

use common::{FakeEmbedder, memory, now};

const OFF: AuthEnabled = AuthEnabled(false);

fn create_params(agent: Id, title: &str, level: &str, parent: Option<Id>) -> WorkCreateToolParams {
    WorkCreateToolParams {
        title: title.to_string(),
        body: None,
        level: level.to_string(),
        parent_id: parent.map(|id| id.to_string()),
        ordinal: None,
        target_namespace: None,
        viewer: Some(format!("agent:{agent}")),
        principal: None,
        teams: Vec::new(),
    }
}

fn advance_params(
    agent: Id,
    work_id: Id,
    to: &str,
    expected_from: Option<&str>,
) -> WorkAdvanceToolParams {
    WorkAdvanceToolParams {
        work_id: work_id.to_string(),
        to: to.to_string(),
        expected_from: expected_from.map(str::to_string),
        viewer: Some(format!("agent:{agent}")),
        principal: None,
        teams: Vec::new(),
    }
}

/// The work item id a `[work_*]` receipt reports in its second whitespace field.
fn id_from_receipt(receipt: &str) -> Id {
    let raw = receipt
        .split_whitespace()
        .nth(1)
        .expect("a receipt names the work id in its second field");
    Id::parse(raw).expect("the receipt id parses")
}

fn create(
    memory: &aionforge_engine::Memory<FakeEmbedder>,
    agent: Id,
    title: &str,
    level: &str,
    parent: Option<Id>,
) -> Id {
    let receipt = work_create_tool(
        memory,
        create_params(agent, title, level, parent),
        &now(),
        None,
        OFF,
    )
    .expect("work_create");
    id_from_receipt(&receipt)
}

#[test]
fn create_advance_link_tree_query_round_trip() {
    let memory = memory();
    let alice = Id::generate();

    let root = create(&memory, alice, "ship the work surface", "epic", None);
    let child = create(&memory, alice, "wire the tools", "task", Some(root));

    // Advance the child Todo -> InProgress as a guarded CAS, and confirm the signed transition.
    let advanced = work_advance_tool(
        &memory,
        advance_params(alice, child, "in_progress", Some("todo")),
        &now(),
        None,
        OFF,
    )
    .expect("work_advance");
    assert!(advanced.contains("status=in_progress"), "{advanced}");
    assert_eq!(
        memory
            .store()
            .audit_count_for_subject(&child)
            .expect("audit count"),
        1,
        "the advance co-commits one signed transition",
    );

    // Tag the child; idempotent on a second identical link.
    let linked = work_link_tool(
        &memory,
        WorkLinkToolParams {
            work_id: child.to_string(),
            slug: "auth".to_string(),
            display: Some("Auth".to_string()),
            viewer: Some(format!("agent:{alice}")),
            principal: None,
            teams: Vec::new(),
        },
        &now(),
        None,
        OFF,
    )
    .expect("work_link");
    assert!(linked.contains("slug=auth"), "{linked}");

    // The tree from the root resolves both nodes (root + child), each a <memory> line.
    let tree = work_tree_tool(
        &memory,
        WorkTreeToolParams {
            root_id: root.to_string(),
            depth: None,
            viewer: Some(format!("agent:{alice}")),
            principal: None,
            teams: Vec::new(),
        },
        None,
        OFF,
    )
    .expect("work_tree");
    assert!(
        tree.starts_with(&format!("[work_tree] root={root} found=2")),
        "{tree}"
    );
    assert!(
        tree.contains(&root.to_string()) && tree.contains(&child.to_string()),
        "{tree}"
    );
    assert_eq!(tree.matches("kind=\"work_item\"").count(), 2, "{tree}");

    // Query by status finds the in-progress child; query by level finds the epic root.
    let by_status = work_query_tool(
        &memory,
        WorkQueryToolParams {
            work_status: Some("in_progress".to_string()),
            level: None,
            limit: None,
            viewer: Some(format!("agent:{alice}")),
            principal: None,
            teams: Vec::new(),
        },
        None,
        OFF,
    )
    .expect("work_query by status");
    // Match the node's OWN id attribute — the root id also appears as the child's parent="...".
    assert!(
        by_status.contains(&format!("id=\"{child}\""))
            && !by_status.contains(&format!("id=\"{root}\"")),
        "{by_status}"
    );

    let by_level = work_query_tool(
        &memory,
        WorkQueryToolParams {
            work_status: None,
            level: Some("epic".to_string()),
            limit: None,
            viewer: Some(format!("agent:{alice}")),
            principal: None,
            teams: Vec::new(),
        },
        None,
        OFF,
    )
    .expect("work_query by level");
    assert!(
        by_level.contains(&format!("id=\"{root}\""))
            && !by_level.contains(&format!("id=\"{child}\"")),
        "{by_level}"
    );
}

#[test]
fn advance_refuses_a_stale_compare_and_set() {
    let memory = memory();
    let alice = Id::generate();
    let item = create(&memory, alice, "t", "task", None);
    // The item is Todo; advancing with expected_from=done is a stale CAS.
    let out = work_advance_tool(
        &memory,
        advance_params(alice, item, "in_progress", Some("done")),
        &now(),
        None,
        OFF,
    );
    assert!(
        out.is_err_and(|e| e.starts_with("ERR_WORK_STATE_CONFLICT")),
        "a stale precondition is a clean conflict",
    );
}

#[test]
fn creating_in_an_unmember_team_namespace_is_refused() {
    let memory = memory();
    let alice = Id::generate();
    let mut params = create_params(alice, "t", "task", None);
    params.target_namespace = Some("team:rocket".to_string());
    // No asserted membership in team:rocket -> the authorizer denies the write.
    let out = work_create_tool(&memory, params, &now(), None, OFF);
    assert!(
        out.is_err_and(|e| e.starts_with("ERR_NOT_AUTHORIZED")),
        "a non-member may not create work in a team namespace",
    );
}

#[test]
fn a_missing_or_cross_namespace_parent_is_refused() {
    let memory = memory();
    let alice = Id::generate();
    let bob = Id::generate();

    // A parent id that resolves to nothing.
    let missing = work_create_tool(
        &memory,
        create_params(alice, "t", "task", Some(Id::generate())),
        &now(),
        None,
        OFF,
    );
    assert!(
        missing.is_err_and(|e| e.starts_with("ERR_WORK_PARENT_NOT_FOUND")),
        "missing parent"
    );

    // A parent in another agent's namespace: a tree may not span namespaces.
    let bob_root = create(&memory, bob, "bob root", "epic", None);
    let cross = work_create_tool(
        &memory,
        create_params(alice, "t", "task", Some(bob_root)),
        &now(),
        None,
        OFF,
    );
    assert!(
        cross.is_err_and(|e| e.starts_with("ERR_WORK_PARENT_NAMESPACE")),
        "cross-namespace parent"
    );
}

#[test]
fn another_agents_work_is_invisible_to_tree_and_query() {
    let memory = memory();
    let alice = Id::generate();
    let bob = Id::generate();
    let bob_item = create(&memory, bob, "bob secret task", "task", None);

    // Alice queries by status: bob's todo item must not surface.
    let q = work_query_tool(
        &memory,
        WorkQueryToolParams {
            work_status: Some("todo".to_string()),
            level: None,
            limit: None,
            viewer: Some(format!("agent:{alice}")),
            principal: None,
            teams: Vec::new(),
        },
        None,
        OFF,
    )
    .expect("work_query");
    assert!(
        q.contains("found=0") && !q.contains(&bob_item.to_string()),
        "{q}"
    );

    // Alice asks for bob's subtree by id: a non-visible root yields an empty tree (absent,
    // never an error that would confirm existence).
    let t = work_tree_tool(
        &memory,
        WorkTreeToolParams {
            root_id: bob_item.to_string(),
            depth: None,
            viewer: Some(format!("agent:{alice}")),
            principal: None,
            teams: Vec::new(),
        },
        None,
        OFF,
    )
    .expect("work_tree");
    assert!(
        t.contains("found=0") && !t.contains("kind=\"work_item\""),
        "{t}"
    );
}

#[test]
fn work_query_requires_a_filter() {
    let memory = memory();
    let alice = Id::generate();
    let out = work_query_tool(
        &memory,
        WorkQueryToolParams {
            work_status: None,
            level: None,
            limit: None,
            viewer: Some(format!("agent:{alice}")),
            principal: None,
            teams: Vec::new(),
        },
        None,
        OFF,
    );
    assert!(
        out.is_err_and(|e| e.starts_with("ERR_WORK_QUERY")),
        "an unfiltered query is refused"
    );
}

#[test]
fn work_tree_with_depth_zero_returns_only_the_root() {
    let memory = memory();
    let alice = Id::generate();
    let root = create(&memory, alice, "root", "epic", None);
    let _child = create(&memory, alice, "child", "task", Some(root));

    let tree = work_tree_tool(
        &memory,
        WorkTreeToolParams {
            root_id: root.to_string(),
            depth: Some(0),
            viewer: Some(format!("agent:{alice}")),
            principal: None,
            teams: Vec::new(),
        },
        None,
        OFF,
    )
    .expect("work_tree");
    assert!(
        tree.starts_with(&format!("[work_tree] root={root} found=1")),
        "{tree}"
    );
    assert_eq!(
        tree.matches("kind=\"work_item\"").count(),
        1,
        "depth=0 collects the root alone: {tree}"
    );
}

#[test]
fn work_query_status_and_level_that_no_item_matches_is_empty() {
    let memory = memory();
    let alice = Id::generate();
    // One Todo task; querying for an in_progress task matches nothing (status narrows it out).
    create(&memory, alice, "t", "task", None);
    let out = work_query_tool(
        &memory,
        WorkQueryToolParams {
            work_status: Some("in_progress".to_string()),
            level: Some("task".to_string()),
            limit: None,
            viewer: Some(format!("agent:{alice}")),
            principal: None,
            teams: Vec::new(),
        },
        None,
        OFF,
    )
    .expect("work_query");
    assert!(
        out.contains("found=0") && !out.contains("kind=\"work_item\""),
        "{out}"
    );
}

#[test]
fn a_read_only_principal_may_not_create_advance_or_link() {
    let memory = memory();
    let alice = Id::generate();
    // Seed a real item to advance/link with a normal (auth-off) write first.
    let item = create(&memory, alice, "t", "task", None);

    let read_only = ValidatedPrincipal::new(
        Principal::agent(alice),
        WritePosture::ReadOnly,
        TokenClass::Spa,
    );

    // With auth ENABLED and a read-only validated identity, every mutating work tool refuses.
    let created = work_create_tool(
        &memory,
        create_params(alice, "t2", "task", None),
        &now(),
        Some(read_only.clone()),
        AuthEnabled(true),
    );
    assert!(
        created.is_err_and(|e| e.starts_with("ERR_READ_ONLY_PRINCIPAL")),
        "create refused"
    );

    let advanced = work_advance_tool(
        &memory,
        advance_params(alice, item, "in_progress", None),
        &now(),
        Some(read_only.clone()),
        AuthEnabled(true),
    );
    assert!(
        advanced.is_err_and(|e| e.starts_with("ERR_READ_ONLY_PRINCIPAL")),
        "advance refused"
    );

    let linked = work_link_tool(
        &memory,
        WorkLinkToolParams {
            work_id: item.to_string(),
            slug: "x".to_string(),
            display: None,
            viewer: None,
            principal: None,
            teams: Vec::new(),
        },
        &now(),
        Some(read_only),
        AuthEnabled(true),
    );
    assert!(
        linked.is_err_and(|e| e.starts_with("ERR_READ_ONLY_PRINCIPAL")),
        "link refused"
    );
}
