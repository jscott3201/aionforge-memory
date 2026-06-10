//! End-to-end tests for the high-precision default path (M2.T08, 03 §4).
//!
//! The path derives a graph candidate seed from the entities a query names, composes it
//! with the current-support set via native set algebra, and exact-vector-reranks the
//! bounded result. The headline scenario is the precision win the spec claims: a current
//! fact about the named entity surfaces even though it sits far from the query in vector
//! space and a plain ANN pass — crowded out by nearer facts about *other* entities —
//! would rank it past the fan-out.
//!
//! Hermetic: a fake embedder maps queries and records to small fixed vectors, so ranking
//! is decided by the seeding and set algebra, not by vector noise.

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
    HybridRetriever, RecallBundle, RecallOptions, RecallQuery, RetrieverConfig, StructuredEntry,
    TemporalMode,
};
use aionforge_store::{NodeId, Store, StoreConfig};

const T0: &str = "2026-01-01T00:00:00Z[UTC]";

// --- Fake embedder ---------------------------------------------------------------

#[derive(Clone)]
struct FakeEmbedder {
    model: EmbedderModel,
    query_vectors: Vec<(String, [f32; 4])>,
    down: bool,
}

impl FakeEmbedder {
    fn new(query_vectors: &[(&str, [f32; 4])]) -> Self {
        Self {
            model: EmbedderModel {
                family: "fake".to_string(),
                version: "1".to_string(),
                dimension: 4,
            },
            query_vectors: query_vectors
                .iter()
                .map(|(q, v)| ((*q).to_string(), *v))
                .collect(),
            down: false,
        }
    }

    fn down(query_vectors: &[(&str, [f32; 4])]) -> Self {
        let mut e = Self::new(query_vectors);
        e.down = true;
        e
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
        let result = if self.down {
            Err(FakeEmbedError)
        } else {
            Ok(inputs
                .iter()
                .map(|input| {
                    let v = self
                        .query_vectors
                        .iter()
                        .find(|(q, _)| q == input)
                        .map(|(_, v)| v.to_vec())
                        .unwrap_or_else(|| vec![1.0, 0.0, 0.0, 0.0]);
                    Embedding::new(v).expect("valid fake embedding")
                })
                .collect())
        };
        async move { result }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

// --- Fixtures --------------------------------------------------------------------

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

fn stats() -> Stats {
    Stats {
        importance: 0.5,
        trust: 0.8,
        last_access: ts(T0),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

fn entity(store: &Store, name: &str, embedding: [f32; 4]) -> (Id, NodeId) {
    let id = Id::generate();
    let entity = Entity {
        identity: Identity {
            id,
            ingested_at: ts(T0),
            namespace: Namespace::Global,
            expired_at: None,
        },
        stats: stats(),
        canonical_name: name.to_string(),
        entity_type: "Concept".to_string(),
        aliases: vec![],
        description: None,
        embedding: Some(Embedding::new(embedding.to_vec()).expect("valid")),
        embedder_model: None,
        attributes: None,
    };
    let node = store.insert_entity(&entity).expect("insert entity");
    (id, node)
}

/// An entity with no embedding — absent from the vector index, so a query never resolves
/// it. Used to force the no-seed dense fallback while the embedder is up.
fn entity_unembedded(store: &Store, name: &str) -> (Id, NodeId) {
    let id = Id::generate();
    let entity = Entity {
        identity: Identity {
            id,
            ingested_at: ts(T0),
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

fn assert_fact(store: &Store, subject: &Id, subject_node: NodeId, statement: &str, emb: [f32; 4]) {
    let fact = Fact {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts(T0),
            namespace: Namespace::Global,
            expired_at: None,
        },
        stats: stats(),
        subject_id: *subject,
        predicate: "based_in".to_string(),
        object: ObjectValue::Text(statement.to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: statement.to_string(),
        embedding: Some(Embedding::new(emb.to_vec()).expect("valid")),
        embedder_model: None,
        extraction: None,
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
        .assert_fact(&fact, subject_node, &about)
        .expect("assert fact");
}

/// The relocation-style fixture for the precision win:
/// - `acme` is the named entity, nearest the query, with ONE far-embedded current fact.
/// - five filler entities sit between acme and `other` in vector space, with no facts, so
///   the top-5 entity resolution is {acme + fillers} and `other` is never resolved.
/// - `other` is far from the query and owns several near-embedded current facts that a
///   plain ANN pass ranks above acme's fact.
///
/// The query vector is [1,0,0,0]; acme=[1,0,0,0], fillers=[0.8,0.6,0,0], other=[0,1,0,0].
fn precision_store() -> Arc<Store> {
    let store = store();

    let (acme, acme_node) = entity(&store, "acme", [1.0, 0.0, 0.0, 0.0]);
    // The target: far from the query in vector space, no lexical overlap with it.
    assert_fact(
        &store,
        &acme,
        acme_node,
        "alpha widget",
        [0.0, 1.0, 0.0, 0.0],
    );

    for n in 0..5 {
        entity(&store, &format!("filler{n}"), [0.8, 0.6, 0.0, 0.0]);
    }

    let (other, other_node) = entity(&store, "other", [0.0, 1.0, 0.0, 0.0]);
    for n in 0..5 {
        assert_fact(
            &store,
            &other,
            other_node,
            &format!("beta widget {n}"),
            [1.0, 0.0, 0.0, 0.0],
        );
    }
    store
}

fn retriever(store: Arc<Store>, embedder: FakeEmbedder) -> HybridRetriever<FakeEmbedder> {
    HybridRetriever::new(store, embedder, RetrieverConfig::default())
}

#[allow(clippy::too_many_arguments)]
async fn recall(
    r: &HybridRetriever<FakeEmbedder>,
    text: &str,
    temporal: TemporalMode,
    sensitive: bool,
) -> RecallBundle {
    r.recall(RecallQuery {
        text: text.to_string(),
        principal: Principal::agent(Id::generate()),
        limit: 3,
        options: RecallOptions {
            temporal,
            sensitive,
            // A tight fan-out so the far target fact would fall outside a plain ANN pass.
            fanout: 3,
            ..RecallOptions::default()
        },
    })
    .await
    .expect("recall")
}

fn has_fact(bundle: &RecallBundle, statement: &str) -> bool {
    bundle.structured.iter().any(|e| match e {
        StructuredEntry::Fact(f) => f.statement == statement,
        _ => false,
    })
}

// --- Tests -----------------------------------------------------------------------

#[tokio::test]
async fn high_precision_path_surfaces_a_current_fact_a_plain_ann_pass_misses() {
    // The query names acme (vector [1,0,0,0] resolves the acme entity), but acme's only
    // current fact sits far away at [0,1,0,0]. The five near distractor facts about
    // `other` (at [1,0,0,0]) would crowd it past the fan-out of 3 in a plain pass.
    let r = retriever(
        precision_store(),
        FakeEmbedder::new(&[("acme", [1.0, 0.0, 0.0, 0.0])]),
    );
    let bundle = recall(&r, "acme", TemporalMode::Current, false).await;

    assert!(
        has_fact(&bundle, "alpha widget"),
        "the seeded current fact about acme surfaces: {}",
        bundle.rendered,
    );
    // None of `other`'s distractor facts are pulled in by the dense signal: the seed
    // narrowed the dense candidate set to acme's facts (other was outside the top-5
    // entity resolution), so they have no dense contribution and do not match lexically.
    assert!(
        !has_fact(&bundle, "beta widget 0"),
        "distractors about an unmentioned entity are not surfaced: {}",
        bundle.rendered,
    );
}

#[tokio::test]
async fn without_the_high_precision_path_the_same_fact_is_missed() {
    // History mode does not fire the §4 path: the fact dense signal is a plain global ANN
    // pass. The far target ranks past the fan-out behind the near distractors, and it has
    // no lexical overlap with the query, so it does not surface — the contrast that
    // isolates the precision win above.
    let r = retriever(
        precision_store(),
        FakeEmbedder::new(&[("acme", [1.0, 0.0, 0.0, 0.0])]),
    );
    let bundle = recall(&r, "acme", TemporalMode::History, false).await;

    assert!(
        !has_fact(&bundle, "alpha widget"),
        "a plain ANN pass misses the far target: {}",
        bundle.rendered,
    );
    assert!(
        has_fact(&bundle, "beta widget 0") || has_fact(&bundle, "beta widget 1"),
        "the near distractors are what a plain pass returns: {}",
        bundle.rendered,
    );
}

#[tokio::test]
async fn an_unavailable_embedder_degrades_to_the_lexical_recall_floor() {
    // With the embedder down there is no query vector, so neither entity resolution nor
    // any dense signal runs. The fact lexical signal over the current set is the recall
    // floor: a current fact whose statement matches the query text still surfaces.
    let store = store();
    let (acme, acme_node) = entity(&store, "acme", [1.0, 0.0, 0.0, 0.0]);
    assert_fact(
        &store,
        &acme,
        acme_node,
        "acme based in berlin",
        [1.0, 0.0, 0.0, 0.0],
    );
    let r = retriever(store, FakeEmbedder::down(&[]));

    let bundle = recall(&r, "berlin", TemporalMode::Current, false).await;
    assert!(
        !bundle.explanation.embedder_available,
        "the outage is flagged"
    );
    assert!(
        has_fact(&bundle, "acme based in berlin"),
        "the lexical recall floor still surfaces the current fact: {}",
        bundle.rendered,
    );
}

#[tokio::test]
async fn sensitive_queries_compose_against_provenance_and_exclude_ungrounded_facts() {
    // acme's fact has an ABOUT window but no incoming support/provenance grounding, so it
    // is in current_support_facts but NOT in provenance_current_support_facts. A sensitive
    // query composes the seed against the provenance set, so the ungrounded fact drops out;
    // a non-sensitive query keeps it. This proves the flag routes to the provenance set.
    let r = retriever(
        precision_store(),
        FakeEmbedder::new(&[("acme", [1.0, 0.0, 0.0, 0.0])]),
    );

    let standard = recall(&r, "acme", TemporalMode::Current, false).await;
    assert!(
        has_fact(&standard, "alpha widget"),
        "a non-sensitive query keeps the ungrounded current fact"
    );

    let sensitive = recall(&r, "acme", TemporalMode::Current, true).await;
    assert!(
        !has_fact(&sensitive, "alpha widget"),
        "a sensitive query excludes the ungrounded fact (provenance set): {}",
        sensitive.rendered,
    );
}

#[tokio::test]
async fn sensitive_scopes_the_dense_fallback_to_provenance_too() {
    // The entity has no embedding, so the query resolves no root and the seed is None: the
    // dense fact ranking takes the no-seed fallback path. That path must still honor
    // `sensitive` — an ungrounded current fact is kept for a normal query but dropped for a
    // sensitive one, which reads the fallback against the provenance set. The fact's
    // statement does not match the query text, so only the dense path decides its presence.
    let store = store();
    let (acme, acme_node) = entity_unembedded(&store, "acme");
    assert_fact(
        &store,
        &acme,
        acme_node,
        "alpha widget",
        [1.0, 0.0, 0.0, 0.0],
    );
    let r = retriever(store, FakeEmbedder::new(&[("acme", [1.0, 0.0, 0.0, 0.0])]));

    let standard = recall(&r, "acme", TemporalMode::Current, false).await;
    assert!(
        has_fact(&standard, "alpha widget"),
        "the no-seed fallback surfaces the ungrounded current fact for a normal query"
    );

    let sensitive = recall(&r, "acme", TemporalMode::Current, true).await;
    assert!(
        !has_fact(&sensitive, "alpha widget"),
        "the no-seed fallback also reads against provenance for a sensitive query: {}",
        sensitive.rendered,
    );
}

#[tokio::test]
async fn the_rendered_view_is_byte_identical_across_runs() {
    let r = retriever(
        precision_store(),
        FakeEmbedder::new(&[("acme", [1.0, 0.0, 0.0, 0.0])]),
    );
    let first = recall(&r, "acme", TemporalMode::Current, false).await;
    let second = recall(&r, "acme", TemporalMode::Current, false).await;
    assert_eq!(
        first.rendered, second.rendered,
        "the high-precision path is deterministic (prefix-cache contract)"
    );
}
