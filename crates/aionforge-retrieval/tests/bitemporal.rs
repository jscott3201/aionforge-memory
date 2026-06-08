//! End-to-end tests for bi-temporal fact retrieval (M2.T07, 03 §5).
//!
//! Hermetic: a fake embedder maps the query to a vector and every seeded record carries
//! the same small vector, so the dense path runs with no network and ranking is decided
//! by the temporal filter rather than by vector noise. The canonical scenario is a
//! relocation — `acme based_in NYC` superseded by `acme based_in SF` — which exercises
//! every mode: Current returns only the live fact, As-of reads the event window, and
//! As-known-at reads the transaction window, while History surfaces both.

use std::future::Future;
use std::sync::Arc;

use aionforge_domain::Retriever;
use aionforge_domain::authz::Principal;
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::edges::{About, SupersededBy};
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

// --- Canonical instants ----------------------------------------------------------

/// NYC becomes the headquarters.
const T1: &str = "2020-01-01T00:00:00Z[UTC]";
/// Between T1 and T2 — NYC is current, SF not yet asserted.
const T1_5: &str = "2021-06-01T00:00:00Z[UTC]";
/// The relocation: SF becomes the headquarters and NYC's window closes here.
const T2: &str = "2023-01-01T00:00:00Z[UTC]";
/// After the relocation.
const T2_5: &str = "2024-01-01T00:00:00Z[UTC]";

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

// --- Fixtures --------------------------------------------------------------------

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

/// Assert `subject predicate object` with the statement `statement`, valid from `from`,
/// carrying the shared fake embedding so the dense signal finds it. Returns the new
/// fact's node id.
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
        predicate: "based_in".to_string(),
        object: ObjectValue::Text(statement.to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: statement.to_string(),
        embedding: Some(Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("valid")),
        embedder_model: None,
        extraction: None,
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

/// The NYC→SF relocation: returns the recall-ready store. NYC is asserted at T1 and
/// superseded by SF at T2 (so NYC's event window closes to T2 and it leaves the
/// current-support set), and an episode is seeded so the mixed render can be exercised.
fn relocation_store() -> Arc<Store> {
    let store = store();
    let (acme, acme_node) = entity(&store, "acme");

    let nyc = assert_fact(&store, &acme, acme_node, "acme based in NYC", T1);
    let sf = assert_fact(&store, &acme, acme_node, "acme based in SF", T2);
    store
        .supersede_fact(
            nyc,
            sf,
            &SupersededBy {
                reason: "headquarters relocated".to_string(),
                temporal: BiTemporal {
                    valid_from: ts(T2),
                    valid_to: None,
                    ingested_at: ts(T2),
                    expired_at: None,
                },
            },
        )
        .expect("supersede");

    seed_episode(&store, "acme office relocation memo");
    store
}

fn seed_episode(store: &Store, content: &str) {
    use aionforge_domain::ids::ContentHash;
    use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};

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
    store.insert_episode(&episode).expect("seed episode");
}

fn retriever() -> HybridRetriever<FakeEmbedder> {
    HybridRetriever::new(
        relocation_store(),
        FakeEmbedder::new(),
        RetrieverConfig::default(),
    )
}

async fn recall(r: &HybridRetriever<FakeEmbedder>, mode: TemporalMode) -> RecallBundle {
    r.recall(RecallQuery {
        text: "acme".to_string(),
        principal: Principal::agent(Id::generate()),
        limit: 10,
        options: RecallOptions {
            temporal: mode,
            ..RecallOptions::default()
        },
    })
    .await
    .expect("recall")
}

/// The fact statements in the bundle (sorted for a stable comparison).
fn fact_statements(bundle: &RecallBundle) -> Vec<String> {
    let mut out: Vec<String> = bundle
        .structured
        .iter()
        .filter_map(|e| match e {
            StructuredEntry::Fact(f) => Some(f.statement.clone()),
            StructuredEntry::Episode(_) => None,
        })
        .collect();
    out.sort();
    out
}

// --- Tests -----------------------------------------------------------------------

#[tokio::test]
async fn current_returns_only_the_live_fact() {
    let bundle = recall(&retriever(), TemporalMode::Current).await;
    assert_eq!(
        fact_statements(&bundle),
        vec!["acme based in SF".to_string()],
        "current excludes the superseded NYC fact",
    );
}

#[tokio::test]
async fn as_of_before_the_move_returns_the_old_fact() {
    let bundle = recall(&retriever(), TemporalMode::AsOf(ts(T1_5))).await;
    assert_eq!(
        fact_statements(&bundle),
        vec!["acme based in NYC".to_string()],
        "as-of inside NYC's event window returns NYC",
    );
}

#[tokio::test]
async fn as_of_after_the_move_returns_the_new_fact() {
    let bundle = recall(&retriever(), TemporalMode::AsOf(ts(T2_5))).await;
    assert_eq!(
        fact_statements(&bundle),
        vec!["acme based in SF".to_string()],
        "as-of after the relocation returns SF",
    );
}

#[tokio::test]
async fn as_of_exactly_at_the_boundary_is_the_new_fact() {
    // valid_to is exclusive and valid_from is inclusive, so the instant of the move
    // belongs to the new fact, not the old one.
    let bundle = recall(&retriever(), TemporalMode::AsOf(ts(T2))).await;
    assert_eq!(
        fact_statements(&bundle),
        vec!["acme based in SF".to_string()],
        "the boundary instant belongs to the successor",
    );
}

#[tokio::test]
async fn as_known_at_reflects_when_each_fact_was_ingested() {
    let r = retriever();
    // At T1.5 the substrate had recorded NYC but not yet SF.
    let early = recall(&r, TemporalMode::AsKnownAt(ts(T1_5))).await;
    assert_eq!(
        fact_statements(&early),
        vec!["acme based in NYC".to_string()],
        "only NYC was known at T1.5",
    );
    // By T2.5 both have been recorded; neither transaction window has been expired.
    let late = recall(&r, TemporalMode::AsKnownAt(ts(T2_5))).await;
    assert_eq!(
        fact_statements(&late),
        vec![
            "acme based in NYC".to_string(),
            "acme based in SF".to_string()
        ],
        "both were known by T2.5",
    );
}

#[tokio::test]
async fn history_returns_every_assertion() {
    let bundle = recall(&retriever(), TemporalMode::History).await;
    assert_eq!(
        fact_statements(&bundle),
        vec![
            "acme based in NYC".to_string(),
            "acme based in SF".to_string()
        ],
        "history surfaces the superseded fact alongside the current one",
    );
}

#[tokio::test]
async fn the_bundle_renders_episodes_and_facts_together() {
    let bundle = recall(&retriever(), TemporalMode::History).await;
    // The mixed bundle carries the episode and both facts.
    assert!(
        bundle
            .structured
            .iter()
            .any(|e| matches!(e, StructuredEntry::Episode(_))),
        "the episode is present",
    );
    assert_eq!(fact_statements(&bundle).len(), 2, "both facts are present");
    // Each kind renders under its own tag, and a fact carries its predicate.
    assert!(
        bundle.rendered.contains("kind=\"episode\""),
        "{}",
        bundle.rendered
    );
    assert!(
        bundle.rendered.contains("kind=\"fact\""),
        "{}",
        bundle.rendered
    );
    assert!(
        bundle.rendered.contains("predicate=\"based_in\""),
        "{}",
        bundle.rendered,
    );
    assert!(bundle.rendered.contains("acme based in SF"));
    assert!(bundle.rendered.contains("acme office relocation memo"));
}

#[tokio::test]
async fn the_rendered_view_is_byte_identical_across_modes_runs() {
    let r = retriever();
    // The same recalled set must render byte-identically across calls — the prefix-cache
    // determinism contract holds with facts in the bundle, not only episodes (03 §6).
    let first = recall(&r, TemporalMode::History).await;
    let second = recall(&r, TemporalMode::History).await;
    assert_eq!(first.rendered, second.rendered);
}
