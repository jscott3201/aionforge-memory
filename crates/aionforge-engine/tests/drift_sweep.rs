//! Acceptance tests for the drift facade (05 §1, M5.T05): the off-switch, the
//! baseline-computation helper, and the end-to-end sweep — score, warning row,
//! anti-flap dedup, and the named-skip tallies.

mod common;

use std::sync::Arc;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::core::{BlockKind, CoreBlock};
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::semantic::{Fact, FactStatus};
use aionforge_domain::value::ObjectValue;
use aionforge_engine::{
    BaselineComputation, CoolingSweepReport, DriftBaseline, DriftPolicy, DriftSweepReport, Memory,
    MemoryConfig,
};
use aionforge_store::Store;

use common::{DIM, FakeEmbedder, migrated_store, ts};

fn drift_memory(store: &Arc<Store>) -> Memory<FakeEmbedder> {
    let config = MemoryConfig {
        drift: DriftPolicy {
            enabled: true,
            min_sample_size: 2,
            behavior_sample_size: 8,
            ..DriftPolicy::default()
        },
        ..MemoryConfig::default()
    };
    Memory::new(Arc::clone(store), FakeEmbedder::new(), config, &ts(0)).expect("memory")
}

/// The live embedder's identity (must match [`FakeEmbedder`]'s).
fn live_model() -> EmbedderModel {
    EmbedderModel {
        family: "fake".to_string(),
        version: "1".to_string(),
        dimension: DIM,
    }
}

fn axis(index: usize) -> Embedding {
    let mut components = vec![0.0f32; DIM as usize];
    components[index] = 1.0;
    Embedding::new(components).expect("finite embedding")
}

fn agent_ns() -> Namespace {
    Namespace::Agent("driftee".to_string())
}

fn stats() -> Stats {
    Stats {
        importance: 0.9,
        trust: 0.9,
        last_access: ts(1),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.0,
        is_pinned: false,
    }
}

/// A baseline anchored on `axis(0)` (the FakeEmbedder direction) whose
/// baseline-time behavior also sat on `axis(0)`: behavior that stays there scores
/// `0.0`, behavior that moves to another axis scores the full `1.0`.
fn sound_baseline(content: &str) -> serde_json::Value {
    DriftBaseline {
        v: DriftBaseline::VERSION,
        embedder_model: live_model(),
        content_hash: ContentHash::of(content.as_bytes()),
        block_embedding: axis(0),
        behavior_centroid: Some(axis(0)),
        baselined_at: ts(2),
        window_secs: 604_800,
        sample_size: 4,
    }
    .to_value()
}

fn block_with(seed: &[u8], content: &str, baseline: Option<serde_json::Value>) -> CoreBlock {
    CoreBlock {
        identity: Identity {
            id: Id::from_content_hash(seed),
            ingested_at: ts(1),
            namespace: agent_ns(),
            expired_at: None,
        },
        stats: stats(),
        content: content.to_string(),
        block_kind: BlockKind::Commitment,
        sensitivity: None,
        drift_baseline: baseline,
        embedding: None,
        embedder_model: None,
    }
}

fn insert_block(store: &Store, block: &CoreBlock) {
    let audit = AuditEvent {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts(1),
            namespace: block.identity.namespace.clone(),
            expired_at: None,
        },
        kind: AuditKind::CoreEdit,
        subject_id: block.identity.id,
        actor_id: Id::from_content_hash(b"test-host"),
        payload: serde_json::json!({"outcome": "created"}),
        signature: String::new(),
        occurred_at: ts(1),
    };
    store
        .create_core_block(block, &audit)
        .expect("create block");
}

fn seed_episode(store: &Store, minute: u32, seed: u8, axis_index: usize) {
    let episode = Episode {
        identity: Identity {
            id: Id::from_content_hash(&[seed]),
            ingested_at: ts(minute),
            namespace: agent_ns(),
            expired_at: None,
        },
        stats: stats(),
        content: format!("episode {seed}"),
        role: Role::User,
        captured_at: ts(minute),
        agent_id: Id::from_content_hash(b"writer"),
        session_id: None,
        content_hash: ContentHash::of(&[seed]),
        embedding: Some(axis(axis_index)),
        embedder_model: Some(live_model()),
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("seed episode");
}

fn drift_warnings(store: &Store) -> Vec<AuditEvent> {
    store
        .audit_by_kind(AuditKind::DriftWarning, None, 50)
        .expect("audit read")
        .events
}

#[tokio::test]
async fn drift_off_is_inert_everywhere() {
    let store = migrated_store();
    let memory = common::memory(&store);
    let report = memory.sweep_drift(None, 100, &ts(30)).expect("sweep");
    assert_eq!(report, DriftSweepReport::default(), "no detector, no read");
    let computed = memory
        .compute_drift_baseline(&Id::from_content_hash(b"any"), &ts(30))
        .await
        .expect("compute");
    assert_eq!(computed, BaselineComputation::Disabled);
}

#[tokio::test]
async fn compute_answers_not_found_for_unknown_and_retired_blocks() {
    let store = migrated_store();
    let memory = drift_memory(&store);
    let computed = memory
        .compute_drift_baseline(&Id::from_content_hash(b"never-created"), &ts(30))
        .await
        .expect("compute");
    assert_eq!(computed, BaselineComputation::NotFound);

    let mut retired = block_with(b"retired", "an abandoned persona", None);
    retired.identity.expired_at = Some(ts(20));
    insert_block(&store, &retired);
    let computed = memory
        .compute_drift_baseline(&retired.identity.id, &ts(30))
        .await
        .expect("compute");
    assert_eq!(
        computed,
        BaselineComputation::NotFound,
        "a retired block gets no baseline proposal"
    );
}

#[tokio::test]
async fn compute_proposes_genesis_then_a_real_centroid_as_behavior_accrues() {
    let store = migrated_store();
    let memory = drift_memory(&store);
    let content = "never deploy on friday";
    let block = block_with(b"commitment", content, None);
    insert_block(&store, &block);

    // No embedded behavior yet: the proposal is a genesis baseline.
    let computed = memory
        .compute_drift_baseline(&block.identity.id, &ts(10))
        .await
        .expect("compute");
    let BaselineComputation::Computed(genesis) = computed else {
        panic!("expected a computed baseline, got {computed:?}");
    };
    assert_eq!(genesis.behavior_centroid, None, "genesis-before-behavior");
    assert_eq!(genesis.sample_size, 0);
    assert_eq!(genesis.embedder_model, live_model());
    assert_eq!(genesis.content_hash, ContentHash::of(content.as_bytes()));
    assert_eq!(
        genesis.block_embedding,
        axis(0),
        "the block content embedded under the live (fake) embedder"
    );

    // Two comparable episodes later, the proposal carries a real centroid.
    seed_episode(&store, 11, 1, 1);
    seed_episode(&store, 12, 2, 1);
    let computed = memory
        .compute_drift_baseline(&block.identity.id, &ts(30))
        .await
        .expect("compute");
    let BaselineComputation::Computed(armed) = computed else {
        panic!("expected a computed baseline, got {computed:?}");
    };
    assert_eq!(armed.behavior_centroid, Some(axis(1)));
    assert_eq!(armed.sample_size, 2);
}

#[tokio::test]
async fn the_sweep_scores_warns_once_and_tallies_every_skip() {
    let store = migrated_store();
    let memory = drift_memory(&store);
    let content = "never deploy on friday";

    // Four blocks, one per outcome: scored-and-crossed, needs-baseline,
    // stale-model, awaiting-first-behavior.
    let drifted = block_with(b"drifted", content, Some(sound_baseline(content)));
    let unseeded = block_with(b"unseeded", "no baseline yet", None);
    let foreign = block_with(
        b"foreign",
        content,
        Some(
            DriftBaseline {
                embedder_model: EmbedderModel {
                    family: "other".to_string(),
                    version: "9".to_string(),
                    dimension: DIM,
                },
                ..serde_json::from_value(sound_baseline(content)).expect("baseline")
            }
            .to_value(),
        ),
    );
    let genesis = block_with(
        b"genesis",
        content,
        Some(
            DriftBaseline {
                behavior_centroid: None,
                sample_size: 0,
                ..serde_json::from_value(sound_baseline(content)).expect("baseline")
            }
            .to_value(),
        ),
    );
    for block in [&drifted, &unseeded, &foreign, &genesis] {
        insert_block(&store, block);
    }
    // Current behavior sits wholly on axis(1); the sound baseline anchored axis(0).
    seed_episode(&store, 10, 1, 1);
    seed_episode(&store, 11, 2, 1);

    let report = memory.sweep_drift(None, 100, &ts(30)).expect("sweep");
    assert_eq!(report.blocks_scanned, 4);
    assert_eq!(report.warnings_emitted, 1, "one crossing, one warning");
    assert_eq!(
        report.max_score,
        Some(1.0),
        "orthogonal movement is full drift"
    );
    assert_eq!(report.baselines_needed, vec![unseeded.identity.id]);
    assert_eq!(report.blocks_stale_model, 1);
    assert_eq!(report.awaiting_first_behavior, 1);
    assert_eq!(report.blocks_skipped, 0);
    assert!(
        report.next.is_some(),
        "a non-empty page returns its watermark"
    );

    let warnings = drift_warnings(&store);
    assert_eq!(warnings.len(), 1);
    let warning = &warnings[0];
    assert_eq!(warning.subject_id, drifted.identity.id);
    assert_eq!(
        warning.identity.namespace,
        agent_ns(),
        "the warning lives in the block's own namespace"
    );
    assert_eq!(warning.payload["score"], serde_json::json!(1.0));
    assert_eq!(warning.payload["sample_size"], serde_json::json!(2));

    // Anti-flap: the same drift against the same baseline epoch warns exactly once.
    let again = memory.sweep_drift(None, 100, &ts(40)).expect("sweep");
    assert_eq!(again.warnings_emitted, 0, "a re-detect dedups to a no-op");
    assert_eq!(
        again.max_score,
        Some(1.0),
        "the score itself is still reported"
    );
    assert_eq!(drift_warnings(&store).len(), 1);
}

#[tokio::test]
async fn steady_behavior_never_warns_and_pages_resume() {
    let store = migrated_store();
    let memory = drift_memory(&store);
    let content = "never deploy on friday";
    let steady = block_with(b"steady", content, Some(sound_baseline(content)));
    let second = block_with(b"second", content, Some(sound_baseline(content)));
    insert_block(&store, &steady);
    insert_block(&store, &second);
    // Behavior still sits on the baseline axis: zero drift.
    seed_episode(&store, 10, 1, 0);
    seed_episode(&store, 11, 2, 0);

    // Page walk with limit 1: two pages, then an empty page ends the scan.
    let first_page = memory.sweep_drift(None, 1, &ts(30)).expect("sweep");
    assert_eq!(first_page.blocks_scanned, 1);
    assert_eq!(first_page.max_score, Some(0.0));
    let cursor = first_page.next.expect("watermark");
    let second_page = memory
        .sweep_drift(Some(&cursor), 1, &ts(30))
        .expect("sweep");
    assert_eq!(second_page.blocks_scanned, 1);
    let cursor = second_page.next.expect("watermark");
    let empty = memory
        .sweep_drift(Some(&cursor), 1, &ts(30))
        .expect("sweep");
    assert_eq!(empty, DriftSweepReport::default(), "the scan completed");

    assert!(
        drift_warnings(&store).is_empty(),
        "zero drift commits no warning row"
    );
}

#[tokio::test]
async fn the_cooling_facade_is_gated_and_stamps_through_the_engine() {
    // Off: no detector, empty report, no read.
    let store = migrated_store();
    let memory = common::memory(&store);
    let report = memory.sweep_cooling(None, 50, &ts(30)).expect("sweep");
    assert_eq!(report, CoolingSweepReport::default());

    // On: a high-trust block anchored on axis(0); a fact embedded there cools.
    let store = migrated_store();
    let memory = drift_memory(&store);
    let content = "never deploy on friday";
    let anchored = block_with(b"anchored", content, Some(sound_baseline(content)));
    insert_block(&store, &anchored);
    let fact = Fact {
        identity: Identity {
            id: Id::from_content_hash(b"proximate-claim"),
            ingested_at: ts(10),
            namespace: agent_ns(),
            expired_at: None,
        },
        stats: stats(),
        subject_id: Id::from_content_hash(b"subject"),
        predicate: "claims".to_string(),
        object: ObjectValue::Text("a proximate claim".to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: "a proximate claim".to_string(),
        embedding: Some(axis(0)),
        embedder_model: Some(live_model()),
        extraction: None,
        cooled_until: None,
    };
    let node = store.insert_fact(&fact).expect("insert fact");

    let report = memory.sweep_cooling(None, 50, &ts(30)).expect("sweep");
    assert_eq!(report.facts_scanned, 1);
    assert_eq!(report.facts_cooled, 1);
    let read = store.fact_by_node_id(node).expect("read").expect("present");
    assert!(
        read.cooled_until.is_some(),
        "the facade stamped the proximate fact"
    );
}
