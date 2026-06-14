//! Regression tests for factual-query lexical anchoring.
//!
//! A broad operational query can surface a precise lexical memory plus dense-near
//! adjacent facts. The factual profile keeps trust, importance, and recency as quality
//! re-ranks, but a top BM25 hit should not sink below unrelated memories that merely
//! collected several weak side signals.

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
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::value::ObjectValue;
use aionforge_retrieval::{
    HybridRetriever, QueryClass, RecallBundle, RecallOptions, RecallQuery, RetrieverConfig, Signal,
    StructuredEntry, classify,
};
use aionforge_store::{NodeId, Store, StoreConfig};

const T0: &str = "2026-01-01T00:00:00Z[UTC]";
const T1: &str = "2026-01-02T00:00:00Z[UTC]";
const T2: &str = "2026-01-03T00:00:00Z[UTC]";
const T3: &str = "2026-01-04T00:00:00Z[UTC]";
const QUERY: &str = "what do we know about disk pressure";
const EXACT: &str =
    "disk pressure memory: keep the WAL volume threshold at eighty percent during service setup";
const ADJACENT_A: &str = "orion allocator uses seeded spans";
const ADJACENT_B: &str = "bravo donor workflow keeps retry budget";
const ADJACENT_C: &str = "apollo merge queue prefers narrow patches";
const NEAR: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
const FAR: [f32; 4] = [0.0, 1.0, 0.0, 0.0];

#[derive(Clone)]
struct FakeEmbedder {
    model: EmbedderModel,
}

#[derive(Debug)]
struct FakeEmbedError;

impl std::fmt::Display for FakeEmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("fake embedder failed")
    }
}

impl std::error::Error for FakeEmbedError {}

impl Embedder for FakeEmbedder {
    type Error = FakeEmbedError;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let embeddings = inputs
            .iter()
            .map(|_| Embedding::new(NEAR.to_vec()).expect("valid fake vector"))
            .collect();
        async move { Ok(embeddings) }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid timestamp")
}

fn store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store.migrate(&ts(T0)).expect("migrate store");
    Arc::new(store)
}

fn embedder() -> FakeEmbedder {
    FakeEmbedder {
        model: EmbedderModel {
            family: "fake".to_string(),
            version: "1".to_string(),
            dimension: 4,
        },
    }
}

fn identity(id: Id, ingested_at: &str) -> Identity {
    Identity {
        id,
        ingested_at: ts(ingested_at),
        namespace: Namespace::Global,
        expired_at: None,
    }
}

fn stats(importance: f64, trust: f64, at: &str) -> Stats {
    Stats {
        importance,
        trust,
        last_access: ts(at),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

fn subject(store: &Store) -> (Id, NodeId) {
    let id = Id::generate();
    let entity = Entity {
        identity: identity(id, T0),
        stats: stats(0.5, 0.5, T0),
        canonical_name: "adjacent operational topic".to_string(),
        entity_type: "Concept".to_string(),
        aliases: vec![],
        description: None,
        embedding: Some(Embedding::new(FAR.to_vec()).expect("valid far vector")),
        embedder_model: None,
        attributes: None,
    };
    let node = store.insert_entity(&entity).expect("insert subject");
    (id, node)
}

fn exact_episode(store: &Store) {
    let episode = Episode {
        identity: identity(Id::generate(), T0),
        stats: stats(0.10, 0.10, T0),
        content: EXACT.to_string(),
        role: Role::User,
        captured_at: ts(T0),
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(EXACT.as_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store
        .insert_episode(&episode)
        .expect("insert exact episode");
}

fn adjacent_fact(
    store: &Store,
    subject_id: Id,
    subject_node: NodeId,
    statement: &str,
    importance: f64,
    trust: f64,
    at: &str,
) {
    let fact = Fact {
        identity: identity(Id::generate(), at),
        stats: stats(importance, trust, at),
        subject_id,
        predicate: "rel".to_string(),
        object: ObjectValue::Text(statement.to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: statement.to_string(),
        embedding: Some(Embedding::new(NEAR.to_vec()).expect("valid near vector")),
        embedder_model: None,
        extraction: None,
        cooled_until: None,
    };
    let about = About {
        temporal: BiTemporal {
            valid_from: ts(at),
            valid_to: None,
            ingested_at: ts(at),
            expired_at: None,
        },
    };
    store
        .assert_fact(&fact, subject_node, &about)
        .expect("assert adjacent fact");
}

fn corpus() -> Arc<Store> {
    let store = store();
    exact_episode(&store);
    let (subject_id, subject_node) = subject(&store);
    adjacent_fact(&store, subject_id, subject_node, ADJACENT_A, 0.95, 0.95, T3);
    adjacent_fact(&store, subject_id, subject_node, ADJACENT_B, 0.90, 0.90, T2);
    adjacent_fact(&store, subject_id, subject_node, ADJACENT_C, 0.85, 0.85, T1);
    store
}

async fn recall() -> RecallBundle {
    let retriever = HybridRetriever::new(corpus(), embedder(), RetrieverConfig::default());
    retriever
        .recall(RecallQuery {
            text: QUERY.to_string(),
            principal: Principal::agent(Id::generate()),
            limit: 10,
            options: RecallOptions {
                fanout: 10,
                now: Some(ts(T3)),
                ..RecallOptions::default()
            },
        })
        .await
        .expect("recall")
}

fn episode_rank(bundle: &RecallBundle, content: &str) -> Option<usize> {
    bundle
        .structured
        .iter()
        .position(|entry| matches!(entry, StructuredEntry::Episode(ep) if ep.content == content))
}

fn fact_rank(bundle: &RecallBundle, statement: &str) -> Option<usize> {
    bundle.structured.iter().position(
        |entry| matches!(entry, StructuredEntry::Fact(fact) if fact.statement == statement),
    )
}

fn episode_signals(bundle: &RecallBundle, content: &str) -> Option<Vec<Signal>> {
    bundle.structured.iter().find_map(|entry| match entry {
        StructuredEntry::Episode(ep) if ep.content == content => {
            Some(ep.contributions.iter().map(|c| c.signal).collect())
        }
        _ => None,
    })
}

#[tokio::test]
async fn factual_queries_anchor_top_lexical_hits_above_adjacent_dense_noise() {
    assert_eq!(classify(QUERY), QueryClass::SingleHopFactual);

    let bundle = recall().await;
    let exact = episode_rank(&bundle, EXACT).expect("exact lexical episode surfaced");
    let adjacent = fact_rank(&bundle, ADJACENT_A).expect("dense-near adjacent fact surfaced");

    assert!(
        exact < adjacent,
        "the exact disk-pressure memory should outrank adjacent dense-near facts \
         (exact #{exact}, adjacent #{adjacent})"
    );
    assert!(
        bundle
            .explanation
            .signals_run
            .contains(&Signal::LexicalAnchor),
        "the explanation reports the lexical-anchor signal"
    );
    assert!(
        episode_signals(&bundle, EXACT).is_some_and(|signals| {
            signals.contains(&Signal::Lexical) && signals.contains(&Signal::LexicalAnchor)
        }),
        "the exact memory carries both lexical and lexical-anchor contributions"
    );
}
