//! Integration test for the community-diversity cap (R2, 03 §6).
//!
//! The community-diversity cap is the structural analogue of the session-diversity cap: where
//! the session cap stops one conversation dominating the bundle, the community cap stops one
//! associative cluster — facts about the same entities — doing the same. It runs native Louvain
//! over the associative projection in `select()` and caps how many members of any one community
//! fill the primary bundle, spilling the rest (topped up only if the bundle is under-filled).
//!
//! Hermetic: a fake embedder puts a dominant 4-fact cluster (entity `alpha`) just above a lone
//! diverse fact (entity `beta`) in dense rank, with no edge bridging the two — so they are
//! distinct Louvain communities. Without the cap the dominant cluster fills a tight bundle and
//! the diverse fact is squeezed out; with the cap at 2 the cluster is held to two and the diverse
//! fact is promoted. That contrast is what proves the cap, not a coincidental ordering.

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
    HybridRetriever, QueryClass, RecallBundle, RecallOptions, RecallQuery, RetrieverConfig,
    StructuredEntry,
};
use aionforge_store::{NodeId, Store, StoreConfig};

const T0: &str = "2026-01-01T00:00:00Z[UTC]";
const QUERY: &str = "topic";
/// The dominant cluster's vector (the query vector): four facts about one entity sit here.
const NEAR: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
/// The diverse fact's vector: a hair off the query so it ranks just below the dominant cluster
/// (cosine ~0.99 > the 0.60 factual floor), making the without-cap squeeze-out deterministic.
const NEARISH: [f32; 4] = [0.9899, 0.1414, 0.0, 0.0];

// --- Fake embedder ---------------------------------------------------------------

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
        // The query embeds to the dominant-cluster vector; the fixtures carry their own stored
        // embeddings, so this is only ever called for the query text.
        let out: Vec<Embedding> = inputs
            .iter()
            .map(|_| Embedding::new(NEAR.to_vec()).expect("valid fake embedding"))
            .collect();
        async move { Ok(out) }
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

fn identity(id: Id) -> Identity {
    Identity {
        id,
        ingested_at: ts(T0),
        namespace: Namespace::Global,
        expired_at: None,
    }
}

fn entity(store: &Store, name: &str) -> (Id, NodeId) {
    let id = Id::generate();
    let entity = Entity {
        identity: identity(id),
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

fn assert_fact(store: &Store, subject: &Id, subject_node: NodeId, statement: &str, vec: [f32; 4]) {
    let fact = Fact {
        identity: identity(Id::generate()),
        stats: stats(),
        subject_id: *subject,
        predicate: "rel".to_string(),
        object: ObjectValue::Text(statement.to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: statement.to_string(),
        embedding: Some(Embedding::new(vec.to_vec()).expect("valid")),
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
        .assert_fact(&fact, subject_node, &about)
        .expect("assert fact");
}

/// Two disconnected associative clusters: `alpha` with four facts (the dominant community, at the
/// query vector) and `beta` with one (the diverse community, a hair below). No edge bridges them,
/// so Louvain places them in different communities.
fn two_cluster_store() -> Arc<Store> {
    let store = store();
    let (alpha, alpha_node) = entity(&store, "alpha");
    for n in 0..4 {
        assert_fact(
            &store,
            &alpha,
            alpha_node,
            &format!("alpha cluster fact {n}"),
            NEAR,
        );
    }
    let (beta, beta_node) = entity(&store, "beta");
    assert_fact(&store, &beta, beta_node, "beta lone fact", NEARISH);
    store
}

async fn recall_with(store: Arc<Store>, community_cap: usize, limit: usize) -> RecallBundle {
    let r = HybridRetriever::new(store, FakeEmbedder::new(), RetrieverConfig::default());
    r.recall(RecallQuery {
        text: QUERY.to_string(),
        principal: Principal::agent(Id::generate()),
        limit,
        options: RecallOptions {
            mode_override: Some(QueryClass::SingleHopFactual),
            community_diversity_cap: community_cap,
            fanout: 10,
            ..RecallOptions::default()
        },
    })
    .await
    .expect("recall")
}

fn cluster_count(bundle: &RecallBundle, prefix: &str) -> usize {
    bundle
        .structured
        .iter()
        .filter(|e| matches!(e, StructuredEntry::Fact(f) if f.statement.starts_with(prefix)))
        .count()
}

#[tokio::test]
async fn the_community_cap_demotes_a_dominant_cluster_and_promotes_the_diverse_one() {
    // Control — no cap: the dominant `alpha` cluster outranks the lone `beta` fact, so a tight
    // 3-slot bundle is all alpha and beta is squeezed out.
    let uncapped = recall_with(two_cluster_store(), 0, 3).await;
    assert_eq!(
        uncapped.structured.len(),
        3,
        "the bundle fills to the limit"
    );
    assert_eq!(
        cluster_count(&uncapped, "alpha cluster"),
        3,
        "without the cap the dominant cluster fills the whole bundle",
    );
    assert_eq!(
        cluster_count(&uncapped, "beta lone"),
        0,
        "without the cap the diverse fact is squeezed out of the tight bundle",
    );

    // With the cap at 2: the dominant cluster is held to two members, and the diverse fact is
    // promoted into the freed slot — same store, same limit, only the cap changed.
    let capped = recall_with(two_cluster_store(), 2, 3).await;
    assert_eq!(
        capped.structured.len(),
        3,
        "the bundle still fills to the limit"
    );
    assert_eq!(
        cluster_count(&capped, "alpha cluster"),
        2,
        "the community cap holds the dominant cluster to two members",
    );
    assert_eq!(
        cluster_count(&capped, "beta lone"),
        1,
        "the diverse community is promoted into the bundle",
    );
}

#[tokio::test]
async fn the_community_cap_is_inert_when_the_bundle_has_room() {
    // A limit wide enough for every fact: the cap never needs to spill, so all five facts are
    // returned regardless of community — diversity is a tie-break under pressure, not a filter.
    let bundle = recall_with(two_cluster_store(), 2, 10).await;
    assert_eq!(
        cluster_count(&bundle, "alpha cluster") + cluster_count(&bundle, "beta lone"),
        5,
        "with room for all, the cap spills nothing (returned {})",
        bundle.structured.len(),
    );
}
