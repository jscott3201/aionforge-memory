//! Tests for MCP lifecycle and audit tool logic.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::time::Timestamp;
use aionforge_engine::{ForgettingPolicy, Memory, MemoryConfig};
use aionforge_mcp::{
    AionforgeMcp, AuditHistoryToolParams, CaptureToolParams, ConsolidationRunToolParams,
    ConsolidationStatusToolParams, MemoryLifecycleToolParams, SearchToolParams, audit_history_tool,
    capture_tool, consolidate_tool, consolidation_status_tool, forget_tool, search_tool,
    unforget_tool,
};
use rmcp::ServiceExt;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone)]
struct FakeEmbedder {
    model: EmbedderModel,
}

impl FakeEmbedder {
    fn new() -> Self {
        Self {
            model: EmbedderModel {
                family: "fake".to_string(),
                version: "1".to_string(),
                dimension: 4,
            },
        }
    }
}

#[derive(Debug)]
struct FakeEmbedError;

impl std::fmt::Display for FakeEmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("fake embedder is down")
    }
}

impl std::error::Error for FakeEmbedError {}

impl Embedder for FakeEmbedder {
    type Error = FakeEmbedError;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let out = inputs
            .iter()
            .map(|_| Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("valid"))
            .collect();
        async move { Ok(out) }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

fn now() -> Timestamp {
    "2026-06-06T09:30:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

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
        agent_id: agent_id.to_string(),
        role: None,
        session_id: None,
        trust: Some(0.1),
        model_family: None,
        captured_at: None,
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
        viewer: format!("agent:{agent}"),
        teams: Vec::new(),
    }
}

fn search_params(query: &str, agent: Id) -> SearchToolParams {
    SearchToolParams {
        query: query.to_string(),
        viewer: format!("agent:{agent}"),
        teams: Vec::new(),
        limit: None,
        verbose: None,
    }
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
        "capture",
        "search",
        "consolidation_status",
        "consolidate",
        "forget",
        "unforget",
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
async fn capture_consolidate_search_forget_cycle_is_client_visible() -> TestResult {
    let memory = forgetting_memory();
    let agent = Id::generate();
    let receipt = capture_tool(
        &memory,
        capture_params("standalone lifecycle memo", &agent.to_string()),
        &now(),
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
    )
    .await?;
    assert!(
        !found.starts_with("hits: 0 "),
        "search sees consolidated memory: {found}"
    );
    let memory_id = first_search_memory_id(&found);

    let forgotten = forget_tool(&memory, lifecycle_params(&memory_id, agent), &now())?;
    assert!(forgotten.contains("outcome=forgotten"), "{forgotten}");
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
    )
    .await?;
    let memory_id = capture_id(&receipt);

    let denied = forget_tool(&memory, lifecycle_params(&memory_id, bob), &now())
        .expect_err("another agent must not point-forget Alice's private memory");
    assert!(denied.starts_with("ERR_NOT_FOUND"), "{denied}");

    let forgotten = forget_tool(&memory, lifecycle_params(&memory_id, alice), &now())?;
    assert!(forgotten.contains("outcome=forgotten"), "{forgotten}");

    let hidden = search_tool(&memory, search_params("lifecycle", alice), &now()).await?;
    assert!(
        hidden.starts_with("hits: 0 "),
        "forgotten memory leaves default recall: {hidden}"
    );

    let audit = audit_history_tool(
        &memory,
        AuditHistoryToolParams {
            subject_id: memory_id.clone(),
            viewer: format!("agent:{alice}"),
            teams: Vec::new(),
            kind: Some("forget".to_string()),
            after: None,
            limit: Some(5),
            verbose: Some(true),
        },
    )?;
    assert!(audit.contains("count=1"), "{audit}");
    assert!(audit.contains("kind=forget"), "{audit}");
    assert!(audit.contains("verification=not_enabled"), "{audit}");

    let restored = unforget_tool(&memory, lifecycle_params(&memory_id, alice), &now())?;
    assert!(restored.contains("outcome=restored"), "{restored}");

    let visible = search_tool(&memory, search_params("lifecycle", alice), &now()).await?;
    assert!(
        visible.starts_with("hits: 1 "),
        "unforget restores default recall: {visible}"
    );
    Ok(())
}

#[tokio::test]
async fn audit_history_rejects_unknown_kind() -> TestResult {
    let memory = memory();
    let agent = Id::generate();
    let err = audit_history_tool(
        &memory,
        AuditHistoryToolParams {
            subject_id: agent.to_string(),
            viewer: format!("agent:{agent}"),
            teams: Vec::new(),
            kind: Some("not_a_kind".to_string()),
            after: None,
            limit: None,
            verbose: None,
        },
    )
    .expect_err("unknown audit kind rejected");
    assert!(err.starts_with("ERR_INVALID_AUDIT_KIND"), "{err}");
    Ok(())
}
