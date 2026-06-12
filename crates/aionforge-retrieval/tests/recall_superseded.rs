//! Superseded episode recall controls.

use std::future::Future;
use std::sync::Arc;

use aionforge_domain::Retriever;
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Origin, Role};
use aionforge_domain::time::Timestamp;
use aionforge_retrieval::{
    HybridRetriever, Principal, RecallQuery, RetrieverConfig, StructuredEntry,
};
use aionforge_store::{Store, StoreConfig};

#[derive(Clone)]
struct FakeEmbedder {
    model: EmbedderModel,
    query: String,
    vector: Vec<f32>,
}

impl FakeEmbedder {
    fn new(query: &str, vector: [f32; 4]) -> Self {
        Self {
            model: EmbedderModel {
                family: "fake".to_string(),
                version: "1".to_string(),
                dimension: 4,
            },
            query: query.to_string(),
            vector: vector.to_vec(),
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
            .map(|input| {
                let vector = if input == &self.query {
                    self.vector.clone()
                } else {
                    vec![1.0, 0.0, 0.0, 0.0]
                };
                Embedding::new(vector).expect("valid fake embedding")
            })
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
    store
        .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
        .expect("migrate store");
    Arc::new(store)
}

fn alice_id() -> Id {
    Id::from_content_hash(b"alice-the-test-reader")
}

fn alice() -> Principal {
    Principal::agent(alice_id())
}

fn alice_ns() -> Namespace {
    Namespace::Agent(alice_id().to_string())
}

fn base_episode(content: &str, ingested_at: &str, embedding: [f32; 4]) -> Episode {
    Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts(ingested_at),
            namespace: alice_ns(),
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
        embedding: Some(Embedding::new(embedding.to_vec()).expect("finite embedding")),
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    }
}

fn seed(store: &Store, content: &str, embedding: [f32; 4]) -> Id {
    let episode = base_episode(
        content,
        "2026-06-06T09:30:00-05:00[America/Chicago]",
        embedding,
    );
    let id = episode.identity.id;
    store.insert_episode(&episode).expect("seed episode");
    id
}

fn seed_superseding(store: &Store, content: &str, target: Id, embedding: [f32; 4]) -> Id {
    let mut episode = base_episode(
        content,
        "2026-06-06T09:31:00-05:00[America/Chicago]",
        embedding,
    );
    episode.captured_at = ts("2026-06-06T09:30:59-05:00[America/Chicago]");
    episode.origin = Some(Origin {
        model_family: Some("test".to_string()),
        model_version: None,
        transport: Some("retrieval-test".to_string()),
        request_id: None,
        redactions: Vec::new(),
        injection_flags: Vec::new(),
        capture_latency_ms: None,
        supersedes: Some(target),
    });
    let id = episode.identity.id;
    store
        .insert_episode(&episode)
        .expect("seed superseding episode");
    id
}

fn retriever(store: Arc<Store>, query: &str) -> HybridRetriever<FakeEmbedder> {
    HybridRetriever::new(
        store,
        FakeEmbedder::new(query, [1.0, 0.0, 0.0, 0.0]),
        RetrieverConfig::default(),
    )
}

#[tokio::test]
async fn recall_can_hide_superseded_episode_evidence() {
    let store = store();
    let query = "superseded recall marker";
    let old_id = seed(
        &store,
        "obsolete superseded recall marker before refresh",
        [1.0, 0.0, 0.0, 0.0],
    );
    let new_id = seed_superseding(
        &store,
        "fresh superseded recall marker after refresh",
        old_id,
        [1.0, 0.0, 0.0, 0.0],
    );
    let r = retriever(store, query);

    let default = r
        .recall(RecallQuery::new(query, alice(), 10))
        .await
        .expect("default recall");
    assert!(
        default.structured.iter().any(|entry| entry.id() == &old_id),
        "default recall preserves superseded evidence"
    );
    assert!(
        default.structured.iter().any(|entry| match entry {
            StructuredEntry::Episode(episode) =>
                episode.id == old_id && episode.superseded_by == Some(new_id),
            _ => false,
        }),
        "default recall annotates the stale episode"
    );

    let mut current_only = RecallQuery::new(query, alice(), 10);
    current_only.options.include_superseded = false;
    let current_only = r.recall(current_only).await.expect("current-only recall");
    assert!(
        current_only
            .structured
            .iter()
            .all(|entry| entry.id() != &old_id),
        "superseded evidence is hidden only when explicitly requested"
    );
}
