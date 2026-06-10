//! End-to-end tests for the soft-forget retrieval gate (05 §2, M5.T02): a forgotten
//! fact leaves every default read, is retained behind `include_expired` in any temporal
//! mode, the bi-temporal window semantics stay untouched (as-known-at reads the ABOUT
//! edge, which a forget never moves), and an un-forget restores recall exactly.
//!
//! Hermetic like the bi-temporal suite: a fake embedder maps everything to one vector,
//! so presence and absence are decided by the gates, never by ranking noise.

use std::future::Future;
use std::sync::Arc;

use aionforge_domain::Retriever;
use aionforge_domain::authz::Principal;
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::edges::About;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::value::ObjectValue;
use aionforge_retrieval::{
    HybridRetriever, RecallBundle, RecallOptions, RecallQuery, RetrieverConfig, TemporalMode,
};
use aionforge_store::{NodeId, Store, StoreConfig};

const T1: &str = "2020-01-01T00:00:00Z[UTC]";
const T1_5: &str = "2021-06-01T00:00:00Z[UTC]";
const T2: &str = "2023-01-01T00:00:00Z[UTC]";
const NOW: &str = "2024-01-01T00:00:00Z[UTC]";

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
struct NeverFails;

impl std::fmt::Display for NeverFails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unreachable")
    }
}

impl std::error::Error for NeverFails {}

impl Embedder for FakeEmbedder {
    type Error = NeverFails;

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

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime")
}

fn store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store.migrate(&ts(T1)).expect("migrate store");
    Arc::new(store)
}

fn stats() -> Stats {
    Stats {
        importance: 0.5,
        trust: 0.8,
        last_access: ts(T1),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

fn entity(store: &Store, name: &str) -> (Id, NodeId) {
    let id = Id::generate();
    let entity = Entity {
        identity: Identity {
            id,
            ingested_at: ts(T1),
            namespace: Namespace::Global,
            expired_at: None,
        },
        stats: stats(),
        canonical_name: name.to_string(),
        entity_type: "Concept".to_string(),
        aliases: vec![],
        description: None,
        embedding: None,
        embedder_model: None,
        attributes: None,
    };
    let node = store.insert_entity(&entity).expect("insert entity");
    (id, node)
}

fn assert_fact(
    store: &Store,
    subject: &Id,
    subject_node: NodeId,
    statement: &str,
    from: &str,
) -> NodeId {
    let fact = Fact {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts(from),
            namespace: Namespace::Global,
            expired_at: None,
        },
        stats: stats(),
        subject_id: *subject,
        predicate: "runs_on".to_string(),
        object: ObjectValue::Text(statement.to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: statement.to_string(),
        embedding: Some(Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("valid")),
        embedder_model: None,
        extraction: None,
        cooled_until: None,
    };
    let about = About {
        temporal: BiTemporal {
            valid_from: ts(from),
            valid_to: None,
            ingested_at: ts(from),
            expired_at: None,
        },
    };
    store
        .assert_fact(&fact, subject_node, &about)
        .expect("assert fact")
}

fn forget_audit(kind: AuditKind, seed: &str) -> AuditEvent {
    AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(seed.as_bytes()),
            ingested_at: ts(NOW),
            namespace: Namespace::Global,
            expired_at: None,
        },
        kind,
        subject_id: Id::from_content_hash(seed.as_bytes()),
        actor_id: Id::from_content_hash(b"test"),
        payload: serde_json::json!({"reason": "test"}),
        signature: String::new(),
        occurred_at: ts(NOW),
    }
}

fn seed_episode(store: &Store, content: &str) -> NodeId {
    let episode = Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts(T1),
            namespace: Namespace::Global,
            expired_at: None,
        },
        stats: stats(),
        content: content.to_string(),
        role: Role::User,
        captured_at: ts(T1),
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: Some(Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("valid")),
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("seed episode")
}

async fn recall(
    r: &HybridRetriever<FakeEmbedder>,
    mode: TemporalMode,
    include_expired: bool,
) -> RecallBundle {
    r.recall(RecallQuery {
        text: "selene".to_string(),
        principal: Principal::agent(Id::generate()),
        limit: 10,
        options: RecallOptions {
            temporal: mode,
            include_expired,
            ..RecallOptions::default()
        },
    })
    .await
    .expect("recall")
}

fn fact_statements(bundle: &RecallBundle) -> Vec<String> {
    let mut out: Vec<String> = bundle
        .structured
        .iter()
        .filter_map(|e| match e {
            aionforge_retrieval::StructuredEntry::Fact(f) => Some(f.statement.clone()),
            _ => None,
        })
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn a_forgotten_fact_leaves_default_recall_and_returns_on_unforget() {
    let store = store();
    let (acme, acme_node) = entity(&store, "acme");
    let victim = assert_fact(&store, &acme, acme_node, "acme runs on selene", T1);
    let bystander = assert_fact(&store, &acme, acme_node, "acme ships selene tools", T2);
    let _ = bystander;
    let r = HybridRetriever::new(
        Arc::clone(&store),
        FakeEmbedder::new(),
        RetrieverConfig::default(),
    );

    let before = recall(&r, TemporalMode::Current, false).await;
    assert_eq!(
        fact_statements(&before),
        vec![
            "acme runs on selene".to_string(),
            "acme ships selene tools".to_string()
        ],
        "both facts current before the forget"
    );

    store
        .soft_forget(victim, &ts(NOW), &forget_audit(AuditKind::Forget, "f1"))
        .expect("forget");

    let after = recall(&r, TemporalMode::Current, false).await;
    assert_eq!(
        fact_statements(&after),
        vec!["acme ships selene tools".to_string()],
        "the forgotten fact left default recall; the bystander stayed"
    );

    // Retained behind include_expired — in Current and in History alike.
    for mode in [TemporalMode::Current, TemporalMode::History] {
        let retained = recall(&r, mode, true).await;
        assert!(
            fact_statements(&retained).contains(&"acme runs on selene".to_string()),
            "include_expired retains the forgotten record"
        );
    }
    // Without the flag, History stays a status/window view, not a forget bypass.
    let history = recall(&r, TemporalMode::History, false).await;
    assert!(
        !fact_statements(&history).contains(&"acme runs on selene".to_string()),
        "soft-forget is out of every default read"
    );

    store
        .unforget(victim, &forget_audit(AuditKind::Unforget, "u1"))
        .expect("unforget");
    let restored = recall(&r, TemporalMode::Current, false).await;
    assert_eq!(
        fact_statements(&restored),
        fact_statements(&before),
        "un-forget restores default recall exactly"
    );
}

#[tokio::test]
async fn the_bitemporal_windows_are_untouched_by_a_forget() {
    let store = store();
    let (acme, acme_node) = entity(&store, "acme");
    let early = assert_fact(&store, &acme, acme_node, "acme runs on selene", T1);
    let _late = assert_fact(&store, &acme, acme_node, "acme ships selene tools", T2);
    let r = HybridRetriever::new(
        Arc::clone(&store),
        FakeEmbedder::new(),
        RetrieverConfig::default(),
    );

    store
        .soft_forget(early, &ts(NOW), &forget_audit(AuditKind::Forget, "f2"))
        .expect("forget");

    // As-known-at with the retention flag reads the same transaction windows as ever:
    // at T1.5 only the early fact was recorded; the forget moved no edge window.
    let early_view = recall(&r, TemporalMode::AsKnownAt(ts(T1_5)), true).await;
    assert_eq!(
        fact_statements(&early_view),
        vec!["acme runs on selene".to_string()],
        "as-known-at window semantics unchanged by the forget"
    );

    // Un-forget and re-read without the flag: byte-identical to a never-forgotten view.
    store
        .unforget(early, &forget_audit(AuditKind::Unforget, "u2"))
        .expect("unforget");
    let full = recall(&r, TemporalMode::AsKnownAt(ts(NOW)), false).await;
    assert_eq!(
        fact_statements(&full),
        vec![
            "acme runs on selene".to_string(),
            "acme ships selene tools".to_string()
        ],
        "after un-forget the as-known-at view is exactly the pre-forget one"
    );
}

#[tokio::test]
async fn the_episode_gate_already_honors_a_soft_forget() {
    let store = store();
    let node = seed_episode(&store, "selene migration notes");
    let r = HybridRetriever::new(
        Arc::clone(&store),
        FakeEmbedder::new(),
        RetrieverConfig::default(),
    );

    let episode_count = |bundle: &RecallBundle| {
        bundle
            .structured
            .iter()
            .filter(|e| matches!(e, aionforge_retrieval::StructuredEntry::Episode(_)))
            .count()
    };

    store
        .soft_forget(node, &ts(NOW), &forget_audit(AuditKind::Forget, "f3"))
        .expect("forget");
    let default_read = recall(&r, TemporalMode::Current, false).await;
    assert_eq!(
        episode_count(&default_read),
        0,
        "a forgotten episode leaves default recall"
    );
    let retained = recall(&r, TemporalMode::Current, true).await;
    assert_eq!(
        episode_count(&retained),
        1,
        "include_expired retains the forgotten episode"
    );
}
