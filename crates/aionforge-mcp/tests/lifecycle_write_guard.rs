//! The read-only write-guard on the forget/unforget/pin/unpin (point-op) path.
//!
//! These ops resolve identity through the *read* scope (`resolve_reader`) and then
//! namespace-authorize the write, so the read-only write-guard does NOT flow through
//! `resolve_writer`. PR4's contract requires that path to apply the SAME shared guard, so a
//! validated read-only/ephemeral identity can never mutate durable memory. Split out of
//! `lifecycle.rs` to keep both files under the 700-LOC cap.

use std::sync::Arc;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::embedding::Embedding;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_engine::{ForgettingPolicy, Memory, MemoryConfig, Principal};
use aionforge_mcp::{
    AuthEnabled, MemoryLifecycleToolParams, TokenClass, ValidatedPrincipal, WritePosture,
    forget_tool, pin_tool, unforget_tool,
};

mod common;

use common::{FakeEmbedder, now};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// A memory whose forgetting policy is permissive enough that a point-forget reaches a real
/// outcome (the write-guard runs regardless of this; it only shapes the writer-path outcome).
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

fn lifecycle_params(memory_id: &str, agent: Id) -> MemoryLifecycleToolParams {
    MemoryLifecycleToolParams {
        memory_id: memory_id.to_string(),
        viewer: Some(format!("agent:{agent}")),
        principal: None,
        teams: Vec::new(),
    }
}

/// Seed one episode straight into the store so a writer-posture op has a real target to mutate.
fn seed_episode(memory: &Memory<FakeEmbedder>, content: &str, namespace: Namespace) -> Id {
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
        role: Role::User,
        captured_at: now(),
        agent_id: Id::generate(),
        session_id: None,
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

/// A validated extension for `agent`, carrying the given write posture (the operator bit and
/// token class are irrelevant to the write-guard, so they are fixed here).
fn validated_extension(agent: Id, posture: WritePosture) -> ValidatedPrincipal {
    ValidatedPrincipal::new(Principal::agent(agent), posture, TokenClass::Spa)
}

#[tokio::test]
async fn a_read_only_extension_refuses_forget_unforget_and_pin_through_the_point_op_path()
-> TestResult {
    // Regression guard: with auth enabled and a ReadOnly validated identity, every point op must
    // refuse with ERR_READ_ONLY_PRINCIPAL — proving `writable_memory` routes through the shared
    // guard. The guard fires before any store lookup, so a non-existent id still yields the
    // read-only error (not ERR_NOT_FOUND): that ordering is what makes this the guard, not the
    // authorizer. A future refactor that deletes or inverts the guard fails here.
    let memory = forgetting_memory();
    let agent = Id::generate();
    let read_only = validated_extension(agent, WritePosture::ReadOnly);
    let bogus_id = Id::generate().to_string();

    let forget_refused = forget_tool(
        &memory,
        lifecycle_params(&bogus_id, agent),
        &now(),
        Some(read_only.clone()),
        AuthEnabled(true),
    )
    .expect_err("a read-only identity may not forget durable memory");
    assert!(
        forget_refused.starts_with("ERR_READ_ONLY_PRINCIPAL"),
        "{forget_refused}"
    );

    let pin_refused = pin_tool(
        &memory,
        lifecycle_params(&bogus_id, agent),
        &now(),
        Some(read_only.clone()),
        AuthEnabled(true),
    )
    .expect_err("a read-only identity may not pin durable memory");
    assert!(
        pin_refused.starts_with("ERR_READ_ONLY_PRINCIPAL"),
        "{pin_refused}"
    );

    let unforget_refused = unforget_tool(
        &memory,
        lifecycle_params(&bogus_id, agent),
        &now(),
        Some(read_only),
        AuthEnabled(true),
    )
    .expect_err("a read-only identity may not unforget durable memory");
    assert!(
        unforget_refused.starts_with("ERR_READ_ONLY_PRINCIPAL"),
        "{unforget_refused}"
    );
    Ok(())
}

#[tokio::test]
async fn a_writer_extension_passes_the_point_op_write_guard() -> TestResult {
    // The dual of the refusal test: a Writer validated identity passes the write-guard and the op
    // proceeds to a real engine outcome. Seed an episode into the extension agent's own private
    // namespace (the extension is authoritative under auth-on) so the namespace authorizer admits
    // the write and the op is not refused at the guard.
    let memory = forgetting_memory();
    let agent = Id::generate();
    let seeded = seed_episode(
        &memory,
        "writer-posture lifecycle memory",
        Namespace::Agent(agent.to_string()),
    );
    let writer = validated_extension(agent, WritePosture::Writer);

    let pinned = pin_tool(
        &memory,
        lifecycle_params(&seeded.to_string(), agent),
        &now(),
        Some(writer.clone()),
        AuthEnabled(true),
    )?;
    assert!(
        pinned.contains("outcome=pinned"),
        "a writer identity pins past the guard: {pinned}"
    );

    let forgotten = forget_tool(
        &memory,
        lifecycle_params(&seeded.to_string(), agent),
        &now(),
        Some(writer),
        AuthEnabled(true),
    )?;
    // The op reached a real engine outcome rather than being refused at the guard — that is the
    // assertion. (A pinned memory is protected from forgetting, so the outcome names that.)
    assert!(
        forgotten.contains("outcome="),
        "a writer identity forgets past the guard: {forgotten}"
    );
    Ok(())
}
