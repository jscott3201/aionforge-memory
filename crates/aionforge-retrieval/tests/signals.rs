//! Integration tests for the lexical and dense retrieval signals (03 §1).
//!
//! Hermetic: documents carry small hand-built vectors and a fake embedder maps the
//! query text to a vector, so the dense path runs with no network. The store is
//! pinned at dimension 4 to match.

use std::future::Future;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::time::Timestamp;
use aionforge_retrieval::{Signal, dense_ranking, lexical_ranking};
use aionforge_store::{NodeId, SearchKind, Store, StoreConfig};

// --- Fake embedder ---------------------------------------------------------------

#[derive(Clone)]
struct FakeEmbedder {
    model: EmbedderModel,
    query_vectors: Vec<(String, Vec<f32>)>,
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
                .map(|(q, v)| ((*q).to_string(), v.to_vec()))
                .collect(),
            down: false,
        }
    }

    fn down() -> Self {
        let mut embedder = Self::new(&[]);
        embedder.down = true;
        embedder
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
                    let vector = self
                        .query_vectors
                        .iter()
                        .find(|(q, _)| q == input)
                        .map(|(_, v)| v.clone())
                        .unwrap_or_else(|| vec![0.0, 0.0, 0.0, 1.0]);
                    Embedding::new(vector).expect("valid fake embedding")
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

fn store() -> Store {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store
        .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
        .expect("migrate store");
    store
}

fn episode(content: &str, embedding: Option<Vec<f32>>) -> Episode {
    Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts("2026-06-06T09:30:00-05:00[America/Chicago]"),
            namespace: Namespace::Agent("alice".to_string()),
            expired_at: None,
        },
        stats: Stats {
            importance: 0.5,
            trust: 0.8,
            last_access: ts("2026-06-06T10:00:00-05:00[America/Chicago]"),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.1,
            is_pinned: false,
        },
        content: content.to_string(),
        role: Role::User,
        captured_at: ts("2026-06-06T09:29:59-05:00[America/Chicago]"),
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: embedding.map(|v| Embedding::new(v).expect("finite embedding")),
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    }
}

fn seed(store: &Store, content: &str, embedding: Vec<f32>) -> NodeId {
    store
        .insert_episode(&episode(content, Some(embedding)))
        .expect("seed embedded episode")
}

fn seed_text(store: &Store, content: &str) -> NodeId {
    store
        .insert_episode(&episode(content, None))
        .expect("seed text episode")
}

// --- Lexical ---------------------------------------------------------------------

#[test]
fn lexical_ranking_returns_matching_docs_best_first() {
    let store = store();
    let g1 = seed_text(&store, "graph retrieval over memory");
    let g2 = seed_text(&store, "a graph of facts");
    let other = seed_text(&store, "completely unrelated text");

    let ranking = lexical_ranking(&store, SearchKind::Episode, "graph", 10, None).expect("lexical");

    assert_eq!(ranking.signal, Signal::Lexical);
    let nodes: Vec<NodeId> = ranking.candidates.iter().map(|c| c.node).collect();
    assert!(
        nodes.contains(&g1) && nodes.contains(&g2),
        "matching docs missing"
    );
    assert!(!nodes.contains(&other), "non-matching doc should not rank");
    // Ranks are dense, 0-based, and ascending in list order.
    for (position, candidate) in ranking.candidates.iter().enumerate() {
        assert_eq!(candidate.rank, position);
    }
}

#[test]
fn lexical_ranking_on_a_kind_without_a_text_index_errors() {
    let store = store();
    let result = lexical_ranking(&store, SearchKind::CoreBlock, "anything", 5, None);
    assert!(result.is_err(), "a kind with no text index must error");
}

// --- Dense -----------------------------------------------------------------------

#[tokio::test]
async fn dense_ranking_orders_by_similarity() {
    let store = store();
    let a = seed(&store, "a", vec![1.0, 0.0, 0.0, 0.0]);
    let c = seed(&store, "c", vec![0.9, 0.1, 0.0, 0.0]);
    let b = seed(&store, "b", vec![0.0, 1.0, 0.0, 0.0]);
    let embedder = FakeEmbedder::new(&[("blue", [1.0, 0.0, 0.0, 0.0])]);

    let dense = dense_ranking(
        &store,
        &embedder,
        SearchKind::Episode,
        "blue",
        10,
        false,
        None,
    )
    .await
    .expect("dense");

    assert!(dense.embedder_available);
    assert_eq!(dense.ranking.signal, Signal::Dense);
    let nodes: Vec<NodeId> = dense.ranking.candidates.iter().map(|c| c.node).collect();
    assert_eq!(
        nodes.first(),
        Some(&a),
        "nearest neighbour should rank first"
    );
    let pos = |n: NodeId| nodes.iter().position(|x| *x == n).expect("present");
    assert!(
        pos(c) < pos(b),
        "the closer vector should outrank the far one"
    );
}

#[tokio::test]
async fn dense_ranking_exact_rerank_keeps_the_nearest_first() {
    let store = store();
    let a = seed(&store, "a", vec![1.0, 0.0, 0.0, 0.0]);
    seed(&store, "b", vec![0.0, 1.0, 0.0, 0.0]);
    let embedder = FakeEmbedder::new(&[("blue", [1.0, 0.0, 0.0, 0.0])]);

    let dense = dense_ranking(
        &store,
        &embedder,
        SearchKind::Episode,
        "blue",
        10,
        true,
        None,
    )
    .await
    .expect("dense with exact rerank");

    assert!(dense.embedder_available);
    assert_eq!(
        dense.ranking.candidates.first().map(|c| c.node),
        Some(a),
        "exact rerank should keep the nearest neighbour first",
    );
}

#[tokio::test]
async fn dense_ranking_degrades_when_the_embedder_is_unavailable() {
    let store = store();
    seed(&store, "a", vec![1.0, 0.0, 0.0, 0.0]);

    let dense = dense_ranking(
        &store,
        &FakeEmbedder::down(),
        SearchKind::Episode,
        "blue",
        10,
        true,
        None,
    )
    .await
    .expect("an unavailable embedder is not an error");

    assert!(!dense.embedder_available, "the embedder was down");
    assert!(
        dense.ranking.candidates.is_empty(),
        "no embedding means no dense hits"
    );
}

#[tokio::test]
async fn dense_ranking_over_an_empty_store_is_empty_but_available() {
    let store = store();
    let embedder = FakeEmbedder::new(&[("blue", [1.0, 0.0, 0.0, 0.0])]);

    let dense = dense_ranking(
        &store,
        &embedder,
        SearchKind::Episode,
        "blue",
        10,
        true,
        None,
    )
    .await
    .expect("dense");

    assert!(dense.embedder_available);
    assert!(dense.ranking.candidates.is_empty());
}
