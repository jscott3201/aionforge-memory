//! Tests for the MCP `audit_history` tool logic.
//!
//! Split out of `lifecycle.rs` so each tool-logic test binary stays within the 700-LOC cap.
//! These cover principal-scoped audit reads: by-kind scans hide another agent's private rows,
//! and malformed queries (unknown kind, no scope, blank subject) are rejected with typed errors.

mod common;

use std::sync::Arc;

use aionforge_domain::ids::Id;
use aionforge_engine::{ForgettingPolicy, Memory, MemoryConfig};
use aionforge_mcp::{
    AuditHistoryToolParams, AuthEnabled, CaptureToolParams, MemoryLifecycleToolParams,
    audit_history_tool, capture_tool, forget_tool,
};

use common::{FakeEmbedder, memory, now};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// A store with active forgetting, so `forget` actually soft-forgets and writes an audit row.
fn forgetting_memory() -> Arc<Memory<FakeEmbedder>> {
    Arc::new(
        Memory::open_in_memory(
            FakeEmbedder::new(),
            &now(),
            MemoryConfig {
                forgetting: ForgettingPolicy {
                    enabled: true,
                    importance_floor: 0.95,
                    trust_floor: 0.95,
                    min_age_secs: 0,
                    ..ForgettingPolicy::default()
                },
                ..MemoryConfig::default()
            },
        )
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

fn capture_id(receipt: &str) -> String {
    receipt
        .split_whitespace()
        .nth(1)
        .expect("compact capture receipt id")
        .to_string()
}

fn lifecycle_params(memory_id: &str, agent: Id) -> MemoryLifecycleToolParams {
    MemoryLifecycleToolParams {
        memory_id: memory_id.to_string(),
        viewer: Some(format!("agent:{agent}")),
        principal: None,
        teams: Vec::new(),
    }
}

#[tokio::test]
async fn audit_history_can_scan_visible_events_by_kind_without_subject() -> TestResult {
    let memory = forgetting_memory();
    let alice = Id::generate();
    let bob = Id::generate();
    let first = capture_tool(
        &memory,
        capture_params("first by-kind audit memory", &alice.to_string()),
        &now(),
        None,
        AuthEnabled(false),
    )
    .await?;
    let second = capture_tool(
        &memory,
        capture_params("second by-kind audit memory", &alice.to_string()),
        &now(),
        None,
        AuthEnabled(false),
    )
    .await?;
    let hidden = capture_tool(
        &memory,
        capture_params("hidden by-kind audit memory", &bob.to_string()),
        &now(),
        None,
        AuthEnabled(false),
    )
    .await?;
    let first_id = capture_id(&first);
    let second_id = capture_id(&second);
    let hidden_id = capture_id(&hidden);
    forget_tool(
        &memory,
        lifecycle_params(&first_id, alice),
        &now(),
        None,
        AuthEnabled(false),
    )?;
    forget_tool(
        &memory,
        lifecycle_params(&second_id, alice),
        &now(),
        None,
        AuthEnabled(false),
    )?;
    forget_tool(
        &memory,
        lifecycle_params(&hidden_id, bob),
        &now(),
        None,
        AuthEnabled(false),
    )?;

    let audit = audit_history_tool(
        &memory,
        AuditHistoryToolParams {
            subject_id: None,
            viewer: Some(format!("agent:{alice}")),
            principal: None,
            teams: Vec::new(),
            kind: Some("forget".to_string()),
            after: None,
            limit: Some(10),
            verbose: None,
        },
        None,
        AuthEnabled(false),
    )?;

    assert!(
        audit.starts_with("[audit] subject=* kind=forget count=2"),
        "{audit}"
    );
    assert!(audit.contains(&format!("subject={first_id}")), "{audit}");
    assert!(audit.contains(&format!("subject={second_id}")), "{audit}");
    assert!(
        !audit.contains(&format!("subject={hidden_id}")),
        "another agent's private audit row stays hidden: {audit}"
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
            subject_id: Some(agent.to_string()),
            viewer: Some(format!("agent:{agent}")),
            principal: None,
            teams: Vec::new(),
            kind: Some("not_a_kind".to_string()),
            after: None,
            limit: None,
            verbose: None,
        },
        None,
        AuthEnabled(false),
    )
    .expect_err("unknown audit kind rejected");
    assert!(err.starts_with("ERR_INVALID_AUDIT_KIND"), "{err}");
    Ok(())
}

#[tokio::test]
async fn audit_history_requires_subject_or_kind() -> TestResult {
    let memory = memory();
    let agent = Id::generate();
    let err = audit_history_tool(
        &memory,
        AuditHistoryToolParams {
            subject_id: None,
            viewer: Some(format!("agent:{agent}")),
            principal: None,
            teams: Vec::new(),
            kind: None,
            after: None,
            limit: None,
            verbose: None,
        },
        None,
        AuthEnabled(false),
    )
    .expect_err("unscoped audit query rejected");
    assert!(err.starts_with("ERR_INVALID_AUDIT_QUERY"), "{err}");
    Ok(())
}

#[tokio::test]
async fn audit_history_rejects_blank_subject_even_with_kind() -> TestResult {
    let memory = memory();
    let agent = Id::generate();
    let err = audit_history_tool(
        &memory,
        AuditHistoryToolParams {
            subject_id: Some(" ".to_string()),
            viewer: Some(format!("agent:{agent}")),
            principal: None,
            teams: Vec::new(),
            kind: Some("forget".to_string()),
            after: None,
            limit: None,
            verbose: None,
        },
        None,
        AuthEnabled(false),
    )
    .expect_err("blank subject should not become a kind-only query");
    assert!(err.starts_with("ERR_INVALID_SUBJECT_ID"), "{err}");
    Ok(())
}
