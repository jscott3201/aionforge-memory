//! Regression guard for the absolute dense-relevance floor vs. the off-topic-trust
//! failure (P4, 03 §2–§3).
//!
//! The failure: on the factual class, `dense` and `trust` are both weighted HEAVY
//! (1.0), and fusion is by rank, not magnitude — so a confident, high-trust, *dense
//! off-topic* memory accumulates a top trust term that lets it outrank a genuinely
//! on-topic hit, even though the dense signal "knew" it was off-topic. The fix is the
//! absolute dense floor in `select()` (an admission gate, not a fusion change): a
//! candidate must clear an absolute dense similarity to be admitted. This pins both
//! halves — that the default (OFF) admits the off-topic hit, and that an active floor
//! rejects it while keeping the on-topic hits.
//!
//! Hermetic: a fake embedder maps the query to NEAR; each fact is stored with an
//! explicit NEAR (on-topic) or FAR (off-topic) embedding, so dense similarity is the
//! only thing that separates topicality. Statements share no token with the query, so
//! the lexical signal stays idle and the dense/trust dynamic is isolated. Subject
//! entities sit at FAR with names the query never uses, so the graph signal stays idle.

use std::future::Future;
use std::sync::Arc;

use aionforge_domain::Retriever;
use aionforge_domain::authz::Principal;
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::edges::About;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::value::ObjectValue;
use aionforge_retrieval::{
    HybridRetriever, QueryClass, RecallBundle, RecallOptions, RecallQuery, RetrieverConfig, Signal,
    StructuredEntry,
};
use aionforge_store::{NodeId, Store, StoreConfig};

const T0: &str = "2026-01-01T00:00:00Z[UTC]";
const QUERY: &str = "the recurring topic";
/// The query direction: on-topic facts embed here.
const NEAR: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
/// Orthogonal to the query: the off-topic fact (and every subject entity) embeds here.
const FAR: [f32; 4] = [0.0, 1.0, 0.0, 0.0];

/// Three on-topic facts (NEAR) at descending trust, none of which shares a query token.
const ON_TOPIC: [(&str, f64); 3] = [
    ("matter alpha", 0.40),
    ("matter beta", 0.30),
    ("matter gamma", 0.20),
];
/// The lowest-trust on-topic fact — the one the off-topic hit must outrank to show P4.
const WEAKEST_ON_TOPIC: &str = "matter gamma";
/// The off-topic fact (FAR) at the highest trust in the corpus.
const OFF_TOPIC: &str = "tangent omega";

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
        f.write_str("unreachable")
    }
}
impl std::error::Error for FakeEmbedError {}

impl Embedder for FakeEmbedder {
    type Error = FakeEmbedError;
    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        // Only the query is embedded at recall time (fact embeddings are stored
        // explicitly below). Mapping it to NEAR makes the NEAR facts dense-relevant and
        // the FAR fact dense-off-topic.
        let out = inputs
            .iter()
            .map(|_| Embedding::new(NEAR.to_vec()).expect("valid"))
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
    store.migrate(&ts(T0)).expect("migrate store");
    Arc::new(store)
}

fn stats(trust: f64) -> Stats {
    Stats {
        importance: 0.5,
        trust,
        last_access: ts(T0),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

fn identity(id: Id) -> Identity {
    Identity {
        id,
        ingested_at: ts(T0),
        namespace: Namespace::Global,
        expired_at: None,
    }
}

/// A subject entity at FAR with a name the query never uses, so it never resolves as a
/// graph seed and the associative signals stay idle.
fn subject(store: &Store) -> (Id, NodeId) {
    let id = Id::generate();
    let ent = Entity {
        identity: identity(id),
        stats: stats(0.5),
        canonical_name: format!("unrelated subject {id}"),
        entity_type: "Concept".to_string(),
        aliases: vec![],
        description: None,
        embedding: Some(Embedding::new(FAR.to_vec()).expect("valid")),
        embedder_model: None,
        attributes: None,
    };
    let node = store.insert_entity(&ent).expect("insert entity");
    (id, node)
}

/// Assert a fact with the given statement, trust, and explicit embedding direction. The
/// statement shares no token with the query, so only the dense signal (via the stored
/// embedding) and trust separate the facts.
fn fact(store: &Store, statement: &str, trust: f64, vector: [f32; 4]) {
    let (subject_id, subject_node) = subject(store);
    let f = Fact {
        identity: identity(Id::generate()),
        stats: stats(trust),
        subject_id,
        predicate: "rel".to_string(),
        object: ObjectValue::Text(statement.to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: statement.to_string(),
        embedding: Some(Embedding::new(vector.to_vec()).expect("valid")),
        embedder_model: None,
        extraction: None,
        cooled_until: None,
    };
    let about = About {
        temporal: BiTemporal {
            valid_from: ts(T0),
            valid_to: None,
            ingested_at: ts(T0),
            expired_at: None,
        },
    };
    store
        .assert_fact(&f, subject_node, &about)
        .expect("assert fact");
}

/// The corpus: three on-topic NEAR facts (created first, so they hold the better dense
/// and node-id ranks) at descending trust, then one off-topic FAR fact at the highest
/// trust (created last, so it holds the *worst* base ranks — only its trust lifts it).
fn corpus() -> Arc<Store> {
    let store = store();
    for (statement, trust) in ON_TOPIC {
        fact(&store, statement, trust, NEAR);
    }
    fact(&store, OFF_TOPIC, 0.95, FAR);
    store
}

fn retriever() -> HybridRetriever<FakeEmbedder> {
    HybridRetriever::new(
        corpus(),
        FakeEmbedder::new(),
        RetrieverConfig {
            default_fanout: 50,
            ..RetrieverConfig::default()
        },
    )
}

async fn recall(r: &HybridRetriever<FakeEmbedder>, min_relevance: Option<f64>) -> RecallBundle {
    r.recall(RecallQuery {
        text: QUERY.to_string(),
        principal: Principal::agent(Id::generate()),
        limit: 20,
        options: RecallOptions {
            // Force the factual class, where dense and trust are both HEAVY — the
            // condition that lets a high-trust off-topic hit compete.
            mode_override: Some(QueryClass::SingleHopFactual),
            fanout: 20,
            min_relevance,
            ..RecallOptions::default()
        },
    })
    .await
    .expect("recall")
}

fn fact_rank(bundle: &RecallBundle, statement: &str) -> Option<usize> {
    bundle
        .structured
        .iter()
        .position(|e| matches!(e, StructuredEntry::Fact(f) if f.statement == statement))
}

fn fact_signals(bundle: &RecallBundle, statement: &str) -> Option<Vec<Signal>> {
    bundle.structured.iter().find_map(|e| match e {
        StructuredEntry::Fact(f) if f.statement == statement => {
            Some(f.contributions.iter().map(|c| c.signal).collect())
        }
        _ => None,
    })
}

#[tokio::test]
async fn disabling_the_floor_reproduces_the_p4_off_topic_admission() {
    // The P4 baseline, shown by explicitly disabling the floor (min_relevance 0.0): the
    // dense-off-topic but high-trust fact is not just present — it outranks a genuinely
    // on-topic hit, purely because trust is weighted as heavily as dense and fusion is by
    // rank. This is the failure the factual-class floor now rejects by default.
    let bundle = recall(&retriever(), Some(0.0)).await;

    let off = fact_rank(&bundle, OFF_TOPIC).expect("off-topic fact surfaced with the floor off");
    let weakest = fact_rank(&bundle, WEAKEST_ON_TOPIC).expect("weakest on-topic fact surfaced");
    assert!(
        off < weakest,
        "with the floor off, the high-trust off-topic hit outranks an on-topic hit \
         (off #{off}, on #{weakest}) — the P4 failure",
    );
    assert!(
        fact_signals(&bundle, OFF_TOPIC).is_some_and(|s| s.contains(&Signal::Trust)),
        "the off-topic hit competes via a Trust contribution",
    );
}

#[tokio::test]
async fn the_factual_class_floor_rejects_the_off_topic_hit_by_default() {
    // The fix, exercised through the per-class default (no per-query override): the
    // single-hop-factual class floors at 0.60, so the off-topic fact — dense similarity 0,
    // orthogonal to the query — is dropped, while every on-topic fact clears the floor and
    // survives. The floor is an admission gate in select(), so fusion stays rank-only.
    let bundle = recall(&retriever(), None).await;

    assert!(
        fact_rank(&bundle, OFF_TOPIC).is_none(),
        "the factual-class default floor rejects the dense-off-topic hit",
    );
    for (statement, _) in ON_TOPIC {
        assert!(
            fact_rank(&bundle, statement).is_some(),
            "the on-topic fact {statement:?} clears the floor and survives",
        );
    }
}
