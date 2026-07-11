//! Review coverage for `memory_census` isolation, migration verification, and parity.

mod read_memory_support;
use read_memory_support::*;

use std::collections::BTreeSet;
use std::sync::Arc;

use aionforge_domain::time::Timestamp;
use aionforge_engine::Memory;
use aionforge_mcp::{
    AionforgeMcp, MemoryCensusCursorToolParam, MemoryCensusToolParams, memory_census_tool,
};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn census_params(agent: Id) -> MemoryCensusToolParams {
    MemoryCensusToolParams {
        viewer: Some(format!("agent:{agent}")),
        principal: None,
        teams: Vec::new(),
        mode: None,
        namespace: None,
        kind: None,
        limit: None,
        after: None,
        include_system: None,
        verbose: None,
    }
}

fn census_params_with_teams(agent: Id, teams: &[&str]) -> MemoryCensusToolParams {
    MemoryCensusToolParams {
        teams: teams.iter().map(ToString::to_string).collect(),
        ..census_params(agent)
    }
}

fn list_params(agent: Id) -> MemoryCensusToolParams {
    MemoryCensusToolParams {
        mode: Some("list".to_string()),
        ..census_params(agent)
    }
}

#[tokio::test]
async fn list_mode_is_scoped_and_does_not_leak_private_bodies_or_structured_records() -> TestResult
{
    let memory = memory();
    let alice = Id::generate();
    let bob = Id::generate();
    let alice_id = seed(
        &memory,
        "alice visible census body",
        Namespace::Agent(alice.to_string()),
        Role::User,
    );
    let bob_id = seed(
        &memory,
        "bob private census body",
        Namespace::Agent(bob.to_string()),
        Role::User,
    );

    let out = memory_census_tool(&memory, list_params(alice), None, AuthEnabled(false))
        .expect("alice list census");
    assert!(
        out.starts_with("[memory_census] mode=list count=1 total_visible=1"),
        "{out}"
    );
    assert_eq!(out.matches("<memory ").count(), 1, "{out}");
    assert!(out.contains(&alice_id.to_string()), "{out}");
    assert!(!out.contains(&bob_id.to_string()), "{out}");
    assert!(!out.contains("bob private census body"), "{out}");

    let (text, structured) = call_census_structured(
        memory.clone(),
        json!({
            "viewer": format!("agent:{alice}"),
            "mode": "list",
        }),
    )
    .await?;
    assert!(text.contains(&alice_id.to_string()), "{text}");
    assert!(!text.contains(&bob_id.to_string()), "{text}");
    let list = structured.get("list").expect("list payload");
    let alice_id_text = alice_id.to_string();
    let bob_id_text = bob_id.to_string();
    assert_eq!(list.get("count").and_then(Value::as_u64), Some(1));
    assert_eq!(list.get("total_visible").and_then(Value::as_u64), Some(1));
    let memories = list
        .get("memories")
        .and_then(Value::as_array)
        .expect("structured memories");
    assert_eq!(memories.len(), 1, "{structured}");
    assert_eq!(
        memories[0].get("id").and_then(Value::as_str),
        Some(alice_id_text.as_str())
    );
    assert_ne!(
        memories[0].get("id").and_then(Value::as_str),
        Some(bob_id_text.as_str())
    );
    assert_eq!(
        memories[0].get("body").and_then(Value::as_str),
        Some("alice visible census body")
    );
    Ok(())
}

#[tokio::test]
async fn list_mode_out_of_scope_namespace_returns_empty_without_oracle() -> TestResult {
    let memory = memory();
    let alice = Id::generate();
    let bob = Id::generate();
    let bob_id = seed(
        &memory,
        "bob private out of scope body",
        Namespace::Agent(bob.to_string()),
        Role::User,
    );

    let mut params = list_params(alice);
    params.namespace = Some(format!("agent:{bob}"));
    let out = memory_census_tool(&memory, params, None, AuthEnabled(false))
        .expect("out-of-scope list census");
    assert!(
        out.starts_with("[memory_census] mode=list count=0 total_visible=0 limit=50 next=none"),
        "{out}"
    );
    assert_eq!(out.matches("<memory ").count(), 0, "{out}");
    assert!(!out.contains(&bob_id.to_string()), "{out}");
    assert!(!out.contains("bob private out of scope body"), "{out}");

    let (_, structured) = call_census_structured(
        memory,
        json!({
            "viewer": format!("agent:{alice}"),
            "mode": "list",
            "namespace": format!("agent:{bob}"),
        }),
    )
    .await?;
    let list = structured.get("list").expect("list payload");
    assert_eq!(list.get("count").and_then(Value::as_u64), Some(0));
    assert_eq!(list.get("total_visible").and_then(Value::as_u64), Some(0));
    assert!(list.get("next").is_some_and(Value::is_null));
    assert!(
        list.get("memories")
            .and_then(Value::as_array)
            .expect("structured memories")
            .is_empty(),
        "{structured}"
    );
    Ok(())
}

#[test]
fn list_mode_can_verify_a_snapshot_migration_by_exact_id_diff() {
    let alice = Id::from_content_hash(b"migration-census-agent");
    let dropped = Id::from_content_hash(b"migration-census-dropped-record");
    let full = memory();
    seed_migration_snapshot(&full, alice, true);
    let full_ids = collect_all_list_ids(&full, alice, &["squad"]);
    assert!(full_ids.contains(&dropped), "baseline includes dropped id");

    let migrated = memory();
    seed_migration_snapshot(&migrated, alice, false);
    let migrated_ids = collect_all_list_ids(&migrated, alice, &["squad"]);

    let diff: BTreeSet<Id> = full_ids
        .symmetric_difference(&migrated_ids)
        .copied()
        .collect();
    assert_eq!(diff, BTreeSet::from([dropped]));
}

#[tokio::test]
async fn count_totals_and_list_ids_are_consistent_for_visible_memory_kinds() -> TestResult {
    let memory = memory();
    let alice = Id::generate();
    let alice_ns = Namespace::Agent(alice.to_string());
    let team_ns = Namespace::Team("squad".to_string());
    seed(&memory, "count list episode", alice_ns.clone(), Role::User);
    seed_fact(
        &memory,
        "count list fact",
        "asserts",
        alice_ns.clone(),
        FactStatus::Active,
        false,
    );
    seed_entity(
        &memory,
        "Widget",
        "count list entity",
        "thing",
        team_ns.clone(),
    );
    seed_note(&memory, "count list note", Namespace::Global);
    seed_skill(
        &memory,
        "count-list-skill",
        "count list skill",
        team_ns.clone(),
        false,
    );
    seed_bad_pattern(&memory, "count list bad pattern", team_ns.clone());
    seed_work_item(
        &memory,
        "task",
        "count list work",
        None,
        WorkStatus::Todo,
        None,
        0,
        alice_ns,
    );

    let (_, counts) = call_census_structured(
        memory.clone(),
        json!({
            "viewer": format!("agent:{alice}"),
            "teams": ["squad"],
        }),
    )
    .await?;
    assert_namespace_and_total_consistency(&counts);

    let (_, list) = call_census_structured(
        memory,
        json!({
            "viewer": format!("agent:{alice}"),
            "teams": ["squad"],
            "mode": "list",
            "limit": 200,
        }),
    )
    .await?;
    let list_kinds = memory_kind_counts_from_list(&list);
    for (field, expected) in memory_kind_fields(&counts) {
        assert_eq!(
            list_kinds.get(field).copied().unwrap_or(0),
            expected,
            "{field} list count matches counts mode"
        );
    }
    Ok(())
}

#[test]
fn system_role_episodes_are_excluded_from_counts_and_list_unless_revealed() {
    let alice = Id::generate();
    let memory = admin_memory(alice);
    let alice_ns = Namespace::Agent(alice.to_string());
    let team_ns = Namespace::Team("squad".to_string());
    let user_id = seed(
        &memory,
        "ordinary same namespace body",
        alice_ns.clone(),
        Role::User,
    );
    let system_id = seed(
        &memory,
        "same namespace system role body one",
        alice_ns.clone(),
        Role::System,
    );
    let second_system_id = seed(
        &memory,
        "same namespace system role body two",
        alice_ns,
        Role::System,
    );
    let team_system_id = seed(&memory, "team system role body", team_ns, Role::System);

    let counts = memory_census_tool(
        &memory,
        census_params_with_teams(alice, &["squad"]),
        None,
        AuthEnabled(false),
    )
    .expect("default counts");
    assert!(
        counts.contains(&format!("namespace=agent:{alice} memories=1 work_items=0")),
        "{counts}"
    );
    assert!(
        counts.contains("namespace=team:squad memories=0 work_items=0"),
        "{counts}"
    );
    assert!(
        counts.contains("kinds=episodes=1 facts=0 entities=0 notes=0 skills=0 bad_patterns=0"),
        "{counts}"
    );

    let mut list_params = census_params_with_teams(alice, &["squad"]);
    list_params.mode = Some("list".to_string());
    let out =
        memory_census_tool(&memory, list_params, None, AuthEnabled(false)).expect("default list");
    assert!(out.contains(&user_id.to_string()), "{out}");
    assert!(!out.contains(&system_id.to_string()), "{out}");
    assert!(!out.contains(&second_system_id.to_string()), "{out}");
    assert!(!out.contains(&team_system_id.to_string()), "{out}");
    assert!(
        !out.contains("same namespace system role body one"),
        "{out}"
    );
    assert!(
        !out.contains("same namespace system role body two"),
        "{out}"
    );
    assert!(!out.contains("team system role body"), "{out}");

    let mut include = census_params_with_teams(alice, &["squad"]);
    include.include_system = Some(true);
    let revealed =
        memory_census_tool(&memory, include, None, AuthEnabled(false)).expect("revealed counts");
    assert!(
        revealed.contains(&format!("namespace=agent:{alice} memories=3 work_items=0")),
        "{revealed}"
    );
    assert!(
        revealed.contains("namespace=team:squad memories=1 work_items=0"),
        "{revealed}"
    );

    let mut list = census_params_with_teams(alice, &["squad"]);
    list.mode = Some("list".to_string());
    list.include_system = Some(true);
    let revealed_list =
        memory_census_tool(&memory, list, None, AuthEnabled(false)).expect("revealed list");
    assert!(
        revealed_list.contains(&user_id.to_string()),
        "{revealed_list}"
    );
    assert!(
        revealed_list.contains(&system_id.to_string()),
        "{revealed_list}"
    );
    assert!(
        revealed_list.contains(&second_system_id.to_string()),
        "{revealed_list}"
    );
    assert!(
        revealed_list.contains(&team_system_id.to_string()),
        "{revealed_list}"
    );
    assert!(
        revealed_list.contains("same namespace system role body one"),
        "{revealed_list}"
    );
    assert!(
        revealed_list.contains("same namespace system role body two"),
        "{revealed_list}"
    );
    assert!(
        revealed_list.contains("team system role body"),
        "{revealed_list}"
    );
}

#[test]
fn list_mode_kind_filter_accepts_fact_aliases_and_rejects_unknown_kinds() {
    let memory = memory();
    let alice = Id::generate();
    let ns = Namespace::Agent(alice.to_string());
    let episode_id = seed(&memory, "kind filter episode body", ns.clone(), Role::User);
    let fact_id = seed_fact(
        &memory,
        "kind filter fact statement",
        "asserts",
        ns,
        FactStatus::Active,
        false,
    );

    for kind in ["fact", "facts"] {
        let mut params = list_params(alice);
        params.kind = Some(kind.to_string());
        let out =
            memory_census_tool(&memory, params, None, AuthEnabled(false)).expect("fact kind list");
        assert!(
            out.starts_with("[memory_census] mode=list count=1 total_visible=1"),
            "{out}"
        );
        assert_eq!(out.matches("<memory ").count(), 1, "{out}");
        assert!(out.contains(&fact_id.to_string()), "{out}");
        assert!(!out.contains(&episode_id.to_string()), "{out}");
        assert!(!out.contains("kind filter episode body"), "{out}");
    }

    let mut bogus = list_params(alice);
    bogus.kind = Some("bogus".to_string());
    let err = memory_census_tool(&memory, bogus, None, AuthEnabled(false))
        .expect_err("unknown kind rejected");
    assert!(err.starts_with("ERR_INVALID_MEMORY_KIND"), "{err}");
}

#[test]
fn list_mode_keyset_order_uses_chronological_instants_across_zones() {
    let memory = memory();
    let alice = Id::generate();
    let ns = Namespace::Agent(alice.to_string());
    let early_id = Id::from_content_hash(b"early-instant-later-local-clock");
    let late_id = Id::from_content_hash(b"late-instant-earlier-local-clock");
    seed_with_id_at(
        &memory,
        late_id,
        "late instant but lexically earlier local time",
        ns.clone(),
        Role::User,
        ts("2026-06-06T10:00:00-05:00[America/Chicago]"),
    );
    seed_with_id_at(
        &memory,
        early_id,
        "early instant but lexically later local time",
        ns,
        Role::User,
        ts("2026-06-06T12:00:00+02:00[Europe/Paris]"),
    );

    let mut first_page = list_params(alice);
    first_page.limit = Some(1);
    let out =
        memory_census_tool(&memory, first_page, None, AuthEnabled(false)).expect("first page");
    assert!(
        out.contains(&early_id.to_string()),
        "first page follows UTC instant order, not timestamp string order: {out}"
    );
    assert!(!out.contains(&late_id.to_string()), "{out}");
    let cursor = next_cursor(&out);

    let mut second_page = list_params(alice);
    second_page.limit = Some(1);
    second_page.after = Some(cursor);
    let out =
        memory_census_tool(&memory, second_page, None, AuthEnabled(false)).expect("second page");
    assert!(
        out.contains(&late_id.to_string()),
        "cursor resumes after the UTC-normalized first instant: {out}"
    );
    assert!(!out.contains(&early_id.to_string()), "{out}");
    assert!(out.contains("next=none"), "{out}");
}

fn next_cursor(out: &str) -> MemoryCensusCursorToolParam {
    maybe_next_cursor(out).expect("next cursor")
}

fn maybe_next_cursor(out: &str) -> Option<MemoryCensusCursorToolParam> {
    let token = out
        .split_whitespace()
        .find_map(|part| part.strip_prefix("next="))?;
    if token == "none" {
        return None;
    }
    let (ingested_at, id) = token.split_once('|').expect("cursor separator");
    Some(MemoryCensusCursorToolParam {
        ingested_at: ingested_at.to_string(),
        id: id.to_string(),
    })
}

fn ts(raw: &str) -> Timestamp {
    raw.parse().expect("valid zoned datetime")
}

fn migration_records(alice: Id) -> [(Id, &'static str, Namespace); 4] {
    [
        (
            Id::from_content_hash(b"migration-census-agent-alpha"),
            "migration alpha",
            Namespace::Agent(alice.to_string()),
        ),
        (
            Id::from_content_hash(b"migration-census-dropped-record"),
            "migration dropped",
            Namespace::Team("squad".to_string()),
        ),
        (
            Id::from_content_hash(b"migration-census-agent-beta"),
            "migration beta",
            Namespace::Agent(alice.to_string()),
        ),
        (
            Id::from_content_hash(b"migration-census-team-gamma"),
            "migration gamma",
            Namespace::Team("squad".to_string()),
        ),
    ]
}

fn seed_migration_snapshot(memory: &Memory<FakeEmbedder>, alice: Id, include_dropped: bool) -> Id {
    let dropped = Id::from_content_hash(b"migration-census-dropped-record");
    for (id, body, namespace) in migration_records(alice) {
        if include_dropped || id != dropped {
            seed_with_id_at(memory, id, body, namespace, Role::User, now());
        } else {
            seed_with_id_at_expired(memory, id, body, namespace, Role::User, now(), Some(now()));
        }
    }
    dropped
}

fn collect_all_list_ids(memory: &Memory<FakeEmbedder>, agent: Id, teams: &[&str]) -> BTreeSet<Id> {
    let mut ids = BTreeSet::new();
    let mut after = None;
    loop {
        let mut params = census_params_with_teams(agent, teams);
        params.mode = Some("list".to_string());
        params.limit = Some(2);
        params.after = after;
        let out = memory_census_tool(memory, params, None, AuthEnabled(false)).expect("list page");
        ids.extend(memory_ids_from_text(&out));
        match maybe_next_cursor(&out) {
            Some(cursor) => after = Some(cursor),
            None => return ids,
        }
    }
}

fn memory_ids_from_text(out: &str) -> impl Iterator<Item = Id> + '_ {
    out.lines().filter_map(|line| {
        let (_, after_id) = line.split_once("id=\"")?;
        let (id, _) = after_id.split_once('"')?;
        Id::parse(id).ok()
    })
}

async fn call_census_structured(
    memory: Arc<Memory<FakeEmbedder>>,
    args: Value,
) -> TestResult<(String, Value)> {
    let (server_transport, client_transport) = tokio::io::duplex(32 * 1024);
    let server = AionforgeMcp::new(memory);
    let server_handle = tokio::spawn(async move {
        let service = server.serve(server_transport).await?;
        service.waiting().await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });

    let client = ().serve(client_transport).await?;
    let result = client
        .call_tool(CallToolRequestParams::new("memory_census").with_arguments(object_args(args)))
        .await?;
    let text = first_text(&result);
    let structured = result
        .structured_content
        .clone()
        .expect("memory_census structuredContent");
    client.cancel().await?;
    server_handle.await??;
    Ok((text, structured))
}

fn object_args(value: Value) -> serde_json::Map<String, Value> {
    value.as_object().expect("tool args object").clone()
}

fn first_text(result: &CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|content| content.raw.as_text())
        .map(|text| text.text.to_string())
        .unwrap_or_else(|| format!("{result:?}"))
}

fn assert_namespace_and_total_consistency(counts: &Value) {
    let namespaces = counts
        .get("namespaces")
        .and_then(Value::as_array)
        .expect("namespaces");
    let mut summed_memories = 0;
    let mut summed_work = 0;
    for namespace in namespaces {
        let memories = sum_object(namespace.get("kinds").expect("kinds"));
        let work = sum_object(namespace.get("work_statuses").expect("work_statuses"));
        assert_eq!(
            namespace.get("total").and_then(Value::as_u64),
            Some(memories + work),
            "namespace total sums memory and work cells: {namespace}"
        );
        summed_memories += memories;
        summed_work += work;
    }
    assert_eq!(
        counts
            .pointer("/totals/memories")
            .and_then(Value::as_u64)
            .expect("memory total"),
        summed_memories
    );
    assert_eq!(
        counts
            .pointer("/totals/work_items")
            .and_then(Value::as_u64)
            .expect("work total"),
        summed_work
    );
}

fn memory_kind_fields(counts: &Value) -> Vec<(&'static str, u64)> {
    let kinds = counts.pointer("/totals/kinds").expect("total kinds");
    [
        "episodes",
        "facts",
        "entities",
        "notes",
        "skills",
        "bad_patterns",
    ]
    .into_iter()
    .map(|field| {
        (
            field,
            kinds
                .get(field)
                .and_then(Value::as_u64)
                .unwrap_or_else(|| panic!("{field} count")),
        )
    })
    .collect()
}

fn memory_kind_counts_from_list(list: &Value) -> std::collections::HashMap<&'static str, u64> {
    let mut counts = std::collections::HashMap::new();
    let memories = list
        .pointer("/list/memories")
        .and_then(Value::as_array)
        .expect("list memories");
    for memory in memories {
        let field = match memory.get("kind").and_then(Value::as_str).expect("kind") {
            "episode" => "episodes",
            "fact" => "facts",
            "entity" => "entities",
            "note" => "notes",
            "skill" => "skills",
            "bad_pattern" => "bad_patterns",
            other => panic!("unexpected memory kind {other}"),
        };
        *counts.entry(field).or_insert(0) += 1;
    }
    counts
}

fn sum_object(value: &Value) -> u64 {
    value
        .as_object()
        .expect("object")
        .values()
        .map(|value| value.as_u64().expect("u64"))
        .sum()
}
