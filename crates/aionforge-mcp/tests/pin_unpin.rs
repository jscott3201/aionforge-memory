//! Tests for the MCP `pin` / `unpin` tools (dogfood wishlist item 1, M5.T02 rider).
//!
//! The engine pin/unpin primitives shipped audited and reversible, but nothing over MCP
//! could *set* a pin until these tools — so the protection guarded an unreachable flag.
//! These tests pin the behavior the surface promises: writer-scoped, audited, idempotent,
//! and — unlike forget — available with active forgetting disabled (a pin only ever spares).

use std::future::Future;
use std::sync::Arc;

use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::time::Timestamp;
use aionforge_engine::{Memory, MemoryConfig};
use aionforge_mcp::{
    AuditHistoryToolParams, AuthEnabled, CaptureToolParams, MemoryLifecycleToolParams,
    audit_history_tool, capture_tool, pin_tool, unpin_tool,
};

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

fn memory() -> Arc<Memory<FakeEmbedder>> {
    Arc::new(
        Memory::open_in_memory(FakeEmbedder::new(), &now(), MemoryConfig::default())
            .expect("open memory"),
    )
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

fn lifecycle_params(memory_id: &str, agent: Id) -> MemoryLifecycleToolParams {
    MemoryLifecycleToolParams {
        memory_id: memory_id.to_string(),
        viewer: Some(format!("agent:{agent}")),
        principal: None,
        teams: Vec::new(),
    }
}

fn capture_id(receipt: &str) -> String {
    receipt
        .split_whitespace()
        .nth(1)
        .expect("compact capture receipt id")
        .to_string()
}

fn pin_kind_audit(memory: &Memory<FakeEmbedder>, memory_id: &str, agent: Id, kind: &str) -> String {
    audit_history_tool(
        memory,
        AuditHistoryToolParams {
            subject_id: Some(memory_id.to_string()),
            viewer: Some(format!("agent:{agent}")),
            principal: None,
            teams: Vec::new(),
            kind: Some(kind.to_string()),
            after: None,
            limit: Some(5),
            verbose: None,
        },
        None,
        AuthEnabled(false),
    )
    .expect("audit read")
}

#[tokio::test]
async fn pin_and_unpin_are_scoped_audited_and_idempotent() -> TestResult {
    // Pinning has no off-switch, so a plain (forgetting-disabled) memory still pins.
    let memory = memory();
    let alice = Id::generate();
    let bob = Id::generate();
    let receipt = capture_tool(
        &memory,
        capture_params("pinnable lifecycle memory", &alice.to_string()),
        &now(),
        None,
        AuthEnabled(false),
    )
    .await?;
    let memory_id = capture_id(&receipt);

    // Another agent cannot pin Alice's private memory — the same not-found masking as forget,
    // so an outsider cannot even probe its existence.
    let denied = pin_tool(
        &memory,
        lifecycle_params(&memory_id, bob),
        &now(),
        None,
        AuthEnabled(false),
    )
    .expect_err("another agent must not pin Alice's private memory");
    assert!(denied.starts_with("ERR_NOT_FOUND"), "{denied}");

    // First pin applies and audits; a second pin is an idempotent no-op.
    let pinned = pin_tool(
        &memory,
        lifecycle_params(&memory_id, alice),
        &now(),
        None,
        AuthEnabled(false),
    )?;
    assert!(pinned.contains("outcome=pinned"), "{pinned}");
    let again = pin_tool(
        &memory,
        lifecycle_params(&memory_id, alice),
        &now(),
        None,
        AuthEnabled(false),
    )?;
    assert!(again.contains("outcome=already_pinned"), "{again}");

    // Only the applied transition is audited — one pin row, in Alice's own namespace.
    let pin_audit = pin_kind_audit(&memory, &memory_id, alice, "pin");
    assert!(pin_audit.contains("count=1"), "{pin_audit}");
    assert!(pin_audit.contains("kind=pin"), "{pin_audit}");

    // Unpin lifts the stay and audits; a second unpin is an idempotent no-op.
    let unpinned = unpin_tool(
        &memory,
        lifecycle_params(&memory_id, alice),
        &now(),
        None,
        AuthEnabled(false),
    )?;
    assert!(unpinned.contains("outcome=unpinned"), "{unpinned}");
    let still = unpin_tool(
        &memory,
        lifecycle_params(&memory_id, alice),
        &now(),
        None,
        AuthEnabled(false),
    )?;
    assert!(still.contains("outcome=not_pinned"), "{still}");

    let unpin_audit = pin_kind_audit(&memory, &memory_id, alice, "unpin");
    assert!(unpin_audit.contains("count=1"), "{unpin_audit}");
    assert!(unpin_audit.contains("kind=unpin"), "{unpin_audit}");
    Ok(())
}
