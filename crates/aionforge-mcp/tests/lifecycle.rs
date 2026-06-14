//! Tests for MCP lifecycle and audit tool logic.

use std::collections::BTreeSet;
use std::sync::Arc;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::embedding::Embedding;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::time::Timestamp;
use aionforge_engine::{ForgettingPolicy, Memory, MemoryConfig};
use aionforge_mcp::{
    AionforgeMcp, AuditHistoryToolParams, AuthEnabled, CaptureToolParams,
    ConsolidationRunToolParams, ConsolidationStatusToolParams, MemoryLifecycleToolParams,
    ReadMemoryToolParams, SearchToolParams, SessionManifestToolParams, audit_history_tool,
    capture_tool, consolidate_tool, consolidation_status_tool, forget_tool, read_memory_tool,
    search_tool, session_manifest_tool, unforget_tool,
};
use rmcp::ServiceExt;

mod common;

use common::{FakeEmbedder, now};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn memory_with_config(config: MemoryConfig) -> Arc<Memory<FakeEmbedder>> {
    Arc::new(Memory::open_in_memory(FakeEmbedder::new(), &now(), config).expect("open memory"))
}

fn memory() -> Arc<Memory<FakeEmbedder>> {
    memory_with_config(MemoryConfig::default())
}

fn forgetting_memory() -> Arc<Memory<FakeEmbedder>> {
    memory_with_config(MemoryConfig {
        forgetting: ForgettingPolicy {
            enabled: true,
            importance_floor: 0.95,
            trust_floor: 0.95,
            min_age_secs: 0,
            ..ForgettingPolicy::default()
        },
        ..MemoryConfig::default()
    })
}

fn capture_params(content: &str, agent_id: &str) -> CaptureToolParams {
    CaptureToolParams {
        content: content.to_string(),
        agent_id: Some(agent_id.to_string()),
        principal: None,
        teams: Vec::new(),
        target_namespace: None,
        role: None,
        session_id: None,
        trust: Some(0.1),
        model_family: None,
        captured_at: None,
        supersedes: None,
    }
}

fn capture_id(receipt: &str) -> String {
    receipt
        .split_whitespace()
        .nth(1)
        .expect("compact capture receipt id")
        .to_string()
}

fn first_search_memory_id(output: &str) -> String {
    let marker = "<memory id=\"";
    let start = output.find(marker).expect("search result has a memory id") + marker.len();
    let end = output[start..]
        .find('"')
        .expect("memory id attribute closes");
    output[start..start + end].to_string()
}

fn lifecycle_params(memory_id: &str, agent: Id) -> MemoryLifecycleToolParams {
    MemoryLifecycleToolParams {
        memory_id: memory_id.to_string(),
        viewer: Some(format!("agent:{agent}")),
        principal: None,
        teams: Vec::new(),
    }
}

fn search_params(query: &str, agent: Id) -> SearchToolParams {
    SearchToolParams {
        query: query.to_string(),
        viewer: Some(format!("agent:{agent}")),
        principal: None,
        teams: Vec::new(),
        limit: None,
        verbose: None,
        include_superseded: None,
    }
}

fn read_params(memory_id: &str, agent: Id) -> ReadMemoryToolParams {
    ReadMemoryToolParams {
        memory_ids: vec![memory_id.to_string()],
        viewer: Some(format!("agent:{agent}")),
        principal: None,
        teams: Vec::new(),
        verbose: None,
        full: None,
        include_system: None,
    }
}

fn manifest_params(session_id: Id, agent: Id) -> SessionManifestToolParams {
    SessionManifestToolParams {
        session_id: session_id.to_string(),
        viewer: Some(format!("agent:{agent}")),
        principal: None,
        teams: Vec::new(),
        limit: None,
        after: None,
        include_superseded: None,
        verbose: None,
    }
}

/// Seed one episode straight into the store, bypassing the Capturer (which refuses a
/// system-role write). This is the only way to place a `Role::System` turn in an otherwise
/// visible namespace, which is exactly the read-path exclusion these tests exercise.
fn seed_episode(
    memory: &Memory<FakeEmbedder>,
    content: &str,
    namespace: Namespace,
    role: Role,
    session_id: Option<Id>,
) -> Id {
    let id = Id::generate();
    let episode = Episode {
        identity: Identity {
            id,
            ingested_at: now(),
            namespace,
            expired_at: None,
        },
        stats: Stats {
            importance: 0.5,
            trust: 0.8,
            last_access: now(),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.1,
            is_pinned: false,
        },
        content: content.to_string(),
        role,
        captured_at: now(),
        agent_id: Id::generate(),
        session_id,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: Some(Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("finite")),
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    memory
        .store()
        .insert_episode(&episode)
        .expect("seed episode");
    id
}

#[tokio::test]
async fn read_memory_excludes_a_system_role_episode_by_default() -> TestResult {
    let memory = memory();
    let alice = Id::generate();
    // A system-role turn living in Alice's OWN (visible) namespace: the namespace gate would
    // admit it, so only the role gate keeps it hidden — recall excludes system-role, and
    // read_memory must match (no admin reveal granted by the default authority).
    let system_id = seed_episode(
        &memory,
        "a system directive turn",
        Namespace::Agent(alice.to_string()),
        Role::System,
        None,
    );
    let denied = read_memory_tool(
        &memory,
        read_params(&system_id.to_string(), alice),
        None,
        AuthEnabled(false),
    )?;
    assert!(
        denied.contains("requested=1 found=0"),
        "a system-role episode is not surfaced by default, even in one's own ns: {denied}"
    );
    assert!(!denied.contains("a system directive turn"), "{denied}");

    // The gate is role-specific, not a blanket block: an ordinary turn stays readable.
    let normal_id = seed_episode(
        &memory,
        "an ordinary assistant turn",
        Namespace::Agent(alice.to_string()),
        Role::Assistant,
        None,
    );
    let ok = read_memory_tool(
        &memory,
        read_params(&normal_id.to_string(), alice),
        None,
        AuthEnabled(false),
    )?;
    assert!(ok.contains("requested=1 found=1"), "{ok}");
    assert!(ok.contains("an ordinary assistant turn"), "{ok}");
    Ok(())
}

#[tokio::test]
async fn session_manifest_excludes_system_role_episodes_by_default() -> TestResult {
    let memory = memory();
    let alice = Id::generate();
    let session = Id::generate();
    seed_episode(
        &memory,
        "an ordinary assistant turn",
        Namespace::Agent(alice.to_string()),
        Role::Assistant,
        Some(session),
    );
    seed_episode(
        &memory,
        "a system directive turn",
        Namespace::Agent(alice.to_string()),
        Role::System,
        Some(session),
    );
    let manifest = session_manifest_tool(
        &memory,
        manifest_params(session, alice),
        None,
        AuthEnabled(false),
    )?;
    assert!(
        manifest.contains("count=1"),
        "only the non-system episode is listed: {manifest}"
    );
    assert!(
        manifest.contains("an ordinary assistant turn"),
        "{manifest}"
    );
    assert!(
        !manifest.contains("a system directive turn"),
        "a system-role episode must never appear in a manifest by default: {manifest}"
    );
    Ok(())
}

#[tokio::test]
async fn mcp_transport_lists_lifecycle_tools() -> TestResult {
    let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
    let server = AionforgeMcp::new(memory());
    let server_handle = tokio::spawn(async move {
        let service = server.serve(server_transport).await?;
        service.waiting().await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });

    let client = ().serve(client_transport).await?;
    let tools: BTreeSet<String> = client
        .list_all_tools()
        .await?
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect();
    for name in [
        "server_status",
        "capture",
        "search",
        "read_memory",
        "session_manifest",
        "consolidation_status",
        "consolidate",
        "forget",
        "unforget",
        "pin",
        "unpin",
        "audit_history",
    ] {
        assert!(tools.contains(name), "{name} listed in {tools:?}");
    }

    client.cancel().await?;
    server_handle.await??;
    Ok(())
}

#[tokio::test]
async fn consolidation_status_reports_pending_capture_backlog() -> TestResult {
    let memory = memory();
    let agent = Id::generate();
    capture_tool(
        &memory,
        capture_params("status backlog memory", &agent.to_string()),
        &now(),
        None,
        AuthEnabled(false),
    )
    .await?;

    let out = consolidation_status_tool(
        &memory,
        ConsolidationStatusToolParams {
            verbose: Some(true),
        },
        &now(),
    )?;

    assert!(out.starts_with("[consolidation] "), "{out}");
    assert!(out.contains("pending=1"), "{out}");
    assert!(out.contains("failed=0"), "{out}");
    assert!(out.contains("state=backlog_pending"), "{out}");
    Ok(())
}

#[tokio::test]
async fn capture_receipt_lists_fired_injection_marker_ids() -> TestResult {
    let memory = memory();
    let agent = Id::generate();
    let receipt = capture_tool(
        &memory,
        capture_params(
            "The red-team note quoted ignore previous instructions as a marker example, \
             then kept this explanatory project context.",
            &agent.to_string(),
        ),
        &now(),
        None,
        AuthEnabled(false),
    )
    .await?;

    assert!(
        receipt.contains("flags=1[ignore_or_forget_context]"),
        "{receipt}"
    );
    Ok(())
}

#[tokio::test]
async fn capture_consolidate_search_forget_cycle_is_client_visible() -> TestResult {
    let memory = forgetting_memory();
    let agent = Id::generate();
    let receipt = capture_tool(
        &memory,
        capture_params("standalone lifecycle memo", &agent.to_string()),
        &now(),
        None,
        AuthEnabled(false),
    )
    .await?;
    let captured_episode_id = capture_id(&receipt);

    let before = consolidation_status_tool(
        &memory,
        ConsolidationStatusToolParams {
            verbose: Some(false),
        },
        &now(),
    )?;
    assert!(before.contains("pending=1"), "{before}");
    assert!(
        !before.contains(&captured_episode_id),
        "status remains compact and does not list episode ids: {before}"
    );

    let run = consolidate_tool(
        &memory,
        ConsolidationRunToolParams {
            max_ticks: Some(3),
            verbose: Some(true),
        },
    )
    .await?;
    assert!(run.starts_with("[consolidate] "), "{run}");
    assert!(run.contains("consolidated=1"), "{run}");
    assert!(run.contains("pending_after=0"), "{run}");
    assert!(run.contains("rule_set=deterministic_defaults"), "{run}");

    let after = consolidation_status_tool(
        &memory,
        ConsolidationStatusToolParams {
            verbose: Some(false),
        },
        &now(),
    )?;
    assert!(after.contains("pending=0"), "{after}");

    let found = search_tool(
        &memory,
        search_params("standalone lifecycle", agent),
        &now(),
        None,
        AuthEnabled(false),
    )
    .await?;
    assert!(
        !found.starts_with("hits: 0 "),
        "search sees consolidated memory: {found}"
    );
    let memory_id = first_search_memory_id(&found);

    let forgotten = forget_tool(
        &memory,
        lifecycle_params(&memory_id, agent),
        &now(),
        None,
        AuthEnabled(false),
    )?;
    assert!(forgotten.contains("outcome=forgotten"), "{forgotten}");
    Ok(())
}

#[tokio::test]
async fn disabled_forget_receipts_name_the_config_gate() -> TestResult {
    let memory = memory();
    let agent = Id::generate();
    let receipt = capture_tool(
        &memory,
        capture_params("disabled forgetting receipt memory", &agent.to_string()),
        &now(),
        None,
        AuthEnabled(false),
    )
    .await?;
    let memory_id = capture_id(&receipt);

    let forgotten = forget_tool(
        &memory,
        lifecycle_params(&memory_id, agent),
        &now(),
        None,
        AuthEnabled(false),
    )?;
    assert!(
        forgotten.contains("outcome=disabled reason=forgetting.enabled=false"),
        "{forgotten}"
    );

    let restored = unforget_tool(
        &memory,
        lifecycle_params(&memory_id, agent),
        &now(),
        None,
        AuthEnabled(false),
    )?;
    assert!(
        restored.contains("outcome=disabled reason=forgetting.enabled=false"),
        "{restored}"
    );
    Ok(())
}

#[tokio::test]
async fn read_memory_is_principal_scoped() -> TestResult {
    let memory = memory();
    let alice = Id::generate();
    let bob = Id::generate();
    let receipt = capture_tool(
        &memory,
        capture_params("read-by-id private memory", &alice.to_string()),
        &now(),
        None,
        AuthEnabled(false),
    )
    .await?;
    let memory_id = capture_id(&receipt);

    let own = read_memory_tool(
        &memory,
        read_params(&memory_id, alice),
        None,
        AuthEnabled(false),
    )?;
    assert!(own.starts_with("[read_memory] "), "{own}");
    assert!(own.contains("requested=1 found=1"), "{own}");
    assert!(own.contains("read-by-id private memory"), "{own}");

    let denied = read_memory_tool(
        &memory,
        read_params(&memory_id, bob),
        None,
        AuthEnabled(false),
    )?;
    assert!(denied.contains("requested=1 found=0"), "{denied}");
    assert!(!denied.contains("read-by-id private memory"), "{denied}");
    Ok(())
}

#[tokio::test]
async fn session_manifest_filters_to_visible_namespaces() -> TestResult {
    let memory = memory();
    let session = Id::generate();
    let alice = Id::generate();
    let bob = Id::generate();

    let mut alice_private = capture_params("alice private manifest memory", &alice.to_string());
    alice_private.session_id = Some(session.to_string());
    capture_tool(&memory, alice_private, &now(), None, AuthEnabled(false)).await?;

    let mut bob_private = capture_params("bob private manifest memory", &bob.to_string());
    bob_private.session_id = Some(session.to_string());
    capture_tool(&memory, bob_private, &now(), None, AuthEnabled(false)).await?;

    let mut shared = capture_params("squad shared manifest memory", &alice.to_string());
    shared.session_id = Some(session.to_string());
    shared.teams = vec!["squad".to_string()];
    shared.target_namespace = Some("team:squad".to_string());
    capture_tool(&memory, shared, &now(), None, AuthEnabled(false)).await?;

    let alice_without_team = session_manifest_tool(
        &memory,
        manifest_params(session, alice),
        None,
        AuthEnabled(false),
    )?;
    assert!(
        alice_without_team.contains("count=1"),
        "only Alice private is visible without team assertion: {alice_without_team}"
    );
    assert!(
        alice_without_team.contains("alice private manifest memory"),
        "{alice_without_team}"
    );
    assert!(
        !alice_without_team.contains("bob private manifest memory"),
        "{alice_without_team}"
    );
    assert!(
        !alice_without_team.contains("squad shared manifest memory"),
        "{alice_without_team}"
    );

    let mut alice_with_team = manifest_params(session, alice);
    alice_with_team.teams = vec!["squad".to_string()];
    let alice_with_team =
        session_manifest_tool(&memory, alice_with_team, None, AuthEnabled(false))?;
    assert!(
        alice_with_team.contains("count=2"),
        "Alice sees private plus asserted team, not Bob private: {alice_with_team}"
    );
    assert!(alice_with_team.contains("alice private manifest memory"));
    assert!(alice_with_team.contains("squad shared manifest memory"));
    assert!(!alice_with_team.contains("bob private manifest memory"));

    let bob_manifest = session_manifest_tool(
        &memory,
        manifest_params(session, bob),
        None,
        AuthEnabled(false),
    )?;
    assert!(
        bob_manifest.contains("count=1"),
        "Bob only sees his private memory: {bob_manifest}"
    );
    assert!(bob_manifest.contains("bob private manifest memory"));
    assert!(!bob_manifest.contains("alice private manifest memory"));
    assert!(!bob_manifest.contains("squad shared manifest memory"));
    Ok(())
}

#[tokio::test]
async fn session_manifest_applies_limit_after_visibility_filtering() -> TestResult {
    let memory = memory();
    let session = Id::generate();
    let alice = Id::generate();
    let bob = Id::generate();
    let first: Timestamp = "2026-06-06T09:31:00-05:00[America/Chicago]".parse()?;
    let second: Timestamp = "2026-06-06T09:32:00-05:00[America/Chicago]".parse()?;

    let mut bob_private = capture_params("bob earlier private manifest memory", &bob.to_string());
    bob_private.session_id = Some(session.to_string());
    capture_tool(&memory, bob_private, &first, None, AuthEnabled(false)).await?;

    let mut alice_private =
        capture_params("alice later visible manifest memory", &alice.to_string());
    alice_private.session_id = Some(session.to_string());
    capture_tool(&memory, alice_private, &second, None, AuthEnabled(false)).await?;

    let mut manifest = manifest_params(session, alice);
    manifest.limit = Some(1);
    let output = session_manifest_tool(&memory, manifest, None, AuthEnabled(false))?;
    assert!(output.contains("count=1"), "{output}");
    assert!(
        output.contains("alice later visible manifest memory"),
        "{output}"
    );
    assert!(
        !output.contains("bob earlier private manifest memory"),
        "{output}"
    );
    Ok(())
}

#[tokio::test]
async fn forget_and_unforget_are_scoped_and_audited() -> TestResult {
    let memory = forgetting_memory();
    let alice = Id::generate();
    let bob = Id::generate();
    let receipt = capture_tool(
        &memory,
        capture_params("scoped lifecycle memory", &alice.to_string()),
        &now(),
        None,
        AuthEnabled(false),
    )
    .await?;
    let memory_id = capture_id(&receipt);

    let denied = forget_tool(
        &memory,
        lifecycle_params(&memory_id, bob),
        &now(),
        None,
        AuthEnabled(false),
    )
    .expect_err("another agent must not point-forget Alice's private memory");
    assert!(denied.starts_with("ERR_NOT_FOUND"), "{denied}");

    let forgotten = forget_tool(
        &memory,
        lifecycle_params(&memory_id, alice),
        &now(),
        None,
        AuthEnabled(false),
    )?;
    assert!(forgotten.contains("outcome=forgotten"), "{forgotten}");

    let hidden = search_tool(
        &memory,
        search_params("lifecycle", alice),
        &now(),
        None,
        AuthEnabled(false),
    )
    .await?;
    assert!(
        hidden.starts_with("hits: 0 "),
        "forgotten memory leaves default recall: {hidden}"
    );

    let audit = audit_history_tool(
        &memory,
        AuditHistoryToolParams {
            subject_id: Some(memory_id.clone()),
            viewer: Some(format!("agent:{alice}")),
            principal: None,
            teams: Vec::new(),
            kind: Some("forget".to_string()),
            after: None,
            limit: Some(5),
            verbose: Some(true),
        },
        None,
        AuthEnabled(false),
    )?;
    assert!(audit.contains("count=1"), "{audit}");
    assert!(audit.contains("kind=forget"), "{audit}");
    assert!(audit.contains("verification=not_enabled"), "{audit}");

    let restored = unforget_tool(
        &memory,
        lifecycle_params(&memory_id, alice),
        &now(),
        None,
        AuthEnabled(false),
    )?;
    assert!(restored.contains("outcome=restored"), "{restored}");

    let visible = search_tool(
        &memory,
        search_params("lifecycle", alice),
        &now(),
        None,
        AuthEnabled(false),
    )
    .await?;
    assert!(
        visible.starts_with("hits: 1 "),
        "unforget restores default recall: {visible}"
    );
    Ok(())
}
