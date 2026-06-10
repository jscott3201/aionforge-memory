//! M5.T02 acceptance mapping (05 §2): each clause of the active-forgetting contract,
//! end to end through the engine facade — forget through `Memory::forget` /
//! `Memory::sweep_forgetting`, observe through `Memory::search` and the scoped audit
//! reads.
//!
//! - **AC1** — a forgotten memory leaves default retrieval but stays reachable for
//!   history and audit, per kind (Episode and Fact via search; Skill via its point op
//!   and the procedural gate).
//! - **AC2** — every forget and unforget is audited, in the agent-visible namespace,
//!   and the by-subject query returns the row.
//! - **AC3** — forgetting is reversible before the prune: the round trip restores
//!   default retrieval, and an unforget without a forget is a no-op.
//! - **AC4** — conservative: pinned, high-importance, attested, identity-kind,
//!   promotion-lineage, young, and referenced memories are never forgotten, and the
//!   default-off configuration sweeps nothing.

mod common;

use std::sync::Arc;

use aionforge_domain::authz::Principal;
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::embedding::Embedding;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::AuditKind;
use aionforge_domain::nodes::procedural::Skill;
use aionforge_domain::nodes::semantic::{Fact, FactStatus};
use aionforge_domain::time::Timestamp;
use aionforge_domain::value::ObjectValue;
use aionforge_engine::{
    ForgettingPolicy, Memory, MemoryConfig, PointForget, PointUnforget, RecallOptions, RecallQuery,
    SpareReason, TemporalMode,
};
use aionforge_store::Store;
use common::{DIM, FakeEmbedder, migrated_store, ts};

fn fake_embedding() -> Embedding {
    Embedding::new(vec![1.0; DIM as usize]).expect("valid embedding")
}

fn now() -> Timestamp {
    "2026-06-06T12:00:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn long_ago() -> Timestamp {
    "2025-12-01T09:00:00-06:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn memory(store: &Arc<Store>, enabled: bool) -> Memory<FakeEmbedder> {
    let config = MemoryConfig {
        forgetting: ForgettingPolicy {
            enabled,
            ..ForgettingPolicy::default()
        },
        ..MemoryConfig::default()
    };
    Memory::new(Arc::clone(store), FakeEmbedder::new(), config, &ts(0)).expect("memory")
}

fn low_stats() -> Stats {
    Stats {
        importance: 0.04,
        trust: 0.2,
        last_access: long_ago(),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

fn low_episode(content: &str, namespace: Namespace) -> Episode {
    Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: long_ago(),
            namespace,
            expired_at: None,
        },
        stats: low_stats(),
        content: content.to_string(),
        role: Role::User,
        captured_at: long_ago(),
        agent_id: Id::from_content_hash(b"acceptance-agent"),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: Some(fake_embedding()),
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    }
}

fn low_skill(name: &str) -> Skill {
    let body = format!("{name} body");
    Skill {
        identity: Identity {
            id: Id::generate(),
            ingested_at: long_ago(),
            namespace: Namespace::Global,
            expired_at: None,
        },
        stats: low_stats(),
        name: name.to_string(),
        version: 1,
        description: format!("solves the {name} problem"),
        problem_embedding: Some(fake_embedding()),
        embedder_model: None,
        language: "python".to_string(),
        body: body.clone(),
        params: serde_json::json!({ "type": "object" }),
        preconditions: None,
        postconditions: None,
        capabilities: vec![],
        success_count: 0,
        failure_count: 0,
        mean_latency_ms: None,
        source_hash: ContentHash::of(body.as_bytes()),
        last_success_at: None,
        last_failure_at: None,
        deprecated_at: None,
        induced: false,
    }
}

async fn episode_contents(memory: &Memory<FakeEmbedder>, include_expired: bool) -> Vec<String> {
    let bundle = memory
        .search(RecallQuery {
            text: "acceptance".to_string(),
            principal: Principal::agent(Id::from_content_hash(b"acceptance-agent")),
            limit: 10,
            options: RecallOptions {
                temporal: TemporalMode::Current,
                include_expired,
                ..RecallOptions::default()
            },
        })
        .await
        .expect("search");
    let mut out: Vec<String> = bundle
        .structured
        .iter()
        .map(|e| e.content().to_string())
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn ac1_and_ac3_a_forgotten_memory_leaves_default_recall_and_returns() {
    let store = migrated_store();
    let memory = memory(&store, true);
    let owner = Principal::agent(Id::from_content_hash(b"acceptance-agent"));
    let ns = Namespace::Agent(owner.agent_id.to_string());

    let episode = low_episode("acceptance memo one", ns.clone());
    let keeper = low_episode("acceptance memo two", ns.clone());
    store.insert_episode(&episode).expect("insert");
    store.insert_episode(&keeper).expect("insert");

    // AC3 first leg: unforget on a never-forgotten memory is a no-op.
    assert_eq!(
        memory
            .unforget(&episode.identity.id, &now())
            .expect("unforget"),
        PointUnforget::NotForgotten
    );

    assert_eq!(
        memory.forget(&episode.identity.id, &now()).expect("forget"),
        PointForget::Forgotten
    );
    // AC1: out of default retrieval, retained for history.
    assert_eq!(
        episode_contents(&memory, false).await,
        vec!["acceptance memo two".to_string()],
        "the forgotten episode left default recall"
    );
    assert_eq!(
        episode_contents(&memory, true).await,
        vec![
            "acceptance memo one".to_string(),
            "acceptance memo two".to_string()
        ],
        "history retains the forgotten record"
    );

    // AC3: the round trip restores default retrieval exactly.
    assert_eq!(
        memory
            .unforget(&episode.identity.id, &now())
            .expect("unforget"),
        PointUnforget::Restored
    );
    assert_eq!(
        episode_contents(&memory, false).await,
        vec![
            "acceptance memo one".to_string(),
            "acceptance memo two".to_string()
        ],
        "reversibility: default recall is whole again"
    );
}

#[tokio::test]
async fn ac1_a_skill_point_forget_works_through_the_procedural_gate() {
    let store = migrated_store();
    let memory = memory(&store, true);
    let skill = low_skill("acceptance-skill");
    store.save_skill(&skill, None, &[]).expect("save skill");

    assert_eq!(
        memory.forget(&skill.identity.id, &now()).expect("forget"),
        PointForget::Forgotten,
        "a skill is point-forgettable (the sweep never targets it)"
    );
    let resolved = store
        .memory_by_id(&skill.identity.id, &["Skill"])
        .expect("resolve")
        .expect("found");
    assert!(
        resolved.identity.expired_at.is_some(),
        "the skill carries the soft-forget expiry the procedural gate honors"
    );
    assert_eq!(
        memory
            .unforget(&skill.identity.id, &now())
            .expect("unforget"),
        PointUnforget::Restored
    );
}

#[tokio::test]
async fn ac2_every_transition_is_audited_and_cycle_rows_stay_distinct() {
    let store = migrated_store();
    let memory = memory(&store, true);
    let owner = Principal::agent(Id::from_content_hash(b"acceptance-agent"));
    let ns = Namespace::Agent(owner.agent_id.to_string());
    let episode = low_episode("acceptance cycle", ns.clone());
    store.insert_episode(&episode).expect("insert");

    // forget -> unforget -> forget, at distinct instants: three real decisions.
    let t1 = now();
    let t2: Timestamp = "2026-06-06T12:00:01-05:00[America/Chicago]"
        .parse()
        .unwrap();
    let t3: Timestamp = "2026-06-06T12:00:02-05:00[America/Chicago]"
        .parse()
        .unwrap();
    assert_eq!(
        memory.forget(&episode.identity.id, &t1).expect("forget"),
        PointForget::Forgotten
    );
    assert_eq!(
        memory
            .unforget(&episode.identity.id, &t2)
            .expect("unforget"),
        PointUnforget::Restored
    );
    assert_eq!(
        memory.forget(&episode.identity.id, &t3).expect("re-forget"),
        PointForget::Forgotten
    );
    // A same-instant replay of the last forget is a no-op: no spurious fourth row.
    assert_eq!(
        memory.forget(&episode.identity.id, &t3).expect("replay"),
        PointForget::AlreadyForgotten
    );

    // The agent-visible, by-subject audit history holds exactly the cycle.
    let forgets = memory
        .audit_by_subject_kind(&owner, &episode.identity.id, AuditKind::Forget, None, 10)
        .expect("audit");
    assert_eq!(forgets.records.len(), 2, "two distinct forget decisions");
    let unforgets = memory
        .audit_by_subject_kind(&owner, &episode.identity.id, AuditKind::Unforget, None, 10)
        .expect("audit");
    assert_eq!(unforgets.records.len(), 1, "one unforget between them");
    for record in forgets.records.iter().chain(unforgets.records.iter()) {
        assert_eq!(
            record.event.identity.namespace, ns,
            "audited in the memory's own namespace"
        );
    }
}

#[tokio::test]
async fn ac4_conservative_protections_hold_and_off_sweeps_nothing() {
    let store = migrated_store();

    // Default-off: the sweep is a no-op that reads nothing and the population is
    // untouched even though every candidate is all-axes-low.
    let off = memory(&store, false);
    let fact = Fact {
        identity: Identity {
            id: Id::generate(),
            ingested_at: long_ago(),
            namespace: Namespace::Global,
            expired_at: None,
        },
        stats: low_stats(),
        subject_id: Id::from_content_hash(b"subject"),
        predicate: "tests".to_string(),
        object: ObjectValue::Text("acceptance".to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: "tests acceptance".to_string(),
        embedding: None,
        embedder_model: None,
        extraction: None,
    };
    store.insert_fact(&fact).expect("insert");
    let report = off.sweep_forgetting(None, 200, &now()).expect("sweep");
    assert_eq!(report.scanned, 0, "off reads nothing");

    // Protections, reported by name through the point op.
    let on = memory(&store, true);
    let pinned = Fact {
        identity: Identity {
            id: Id::generate(),
            ingested_at: long_ago(),
            namespace: Namespace::Global,
            expired_at: None,
        },
        stats: Stats {
            is_pinned: true,
            ..low_stats()
        },
        ..fact.clone()
    };
    let young = Fact {
        identity: Identity {
            id: Id::generate(),
            ingested_at: now(),
            namespace: Namespace::Global,
            expired_at: None,
        },
        ..fact.clone()
    };
    let important = Fact {
        identity: Identity {
            id: Id::generate(),
            ingested_at: long_ago(),
            namespace: Namespace::Global,
            expired_at: None,
        },
        stats: Stats {
            importance: 0.9,
            last_access: now(),
            ..low_stats()
        },
        ..fact.clone()
    };
    for f in [&pinned, &young, &important] {
        store.insert_fact(f).expect("insert");
    }
    assert_eq!(
        on.forget(&pinned.identity.id, &now()).expect("call"),
        PointForget::Protected(SpareReason::Pinned)
    );
    assert_eq!(
        on.forget(&young.identity.id, &now()).expect("call"),
        PointForget::Protected(SpareReason::TooYoung)
    );
    assert_eq!(
        on.forget(&important.identity.id, &now()).expect("call"),
        PointForget::Protected(SpareReason::ImportanceHolds)
    );
    // The store-level acceptance for attested/lineage/referenced protections is pinned
    // in the orchestrator suite; here the facade contract is that nothing protected was
    // touched.
    let sweep = on.sweep_forgetting(None, 200, &now()).expect("sweep");
    assert_eq!(
        sweep.forgotten, 1,
        "only the unprotected all-axes-low fact is swept"
    );
    assert_eq!(sweep.spared, 3, "every protection held");
}
