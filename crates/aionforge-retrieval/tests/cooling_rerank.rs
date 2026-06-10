//! Integration tests for the cooling-window trust modulation in recall (05 §1,
//! M5.T05): a fact inside its cooling window ranks by `trust × cooling_factor` —
//! a pure read-time modulation that expires without a write — and the modulation is
//! double-gated exactly like decay: the policy switch *and* a caller-stamped clock.
//!
//! Hermetic, mirroring the trust re-rank tests: one fixed NEAR vector for the query
//! and every fact, far-vector subjects the query never names. The assertions read
//! the **trust signal's own contribution ranks** rather than final fused positions —
//! the modulation's contract is the trust ordering it produces, and the fused
//! position also folds surfacing accidents (which signal happened to find each
//! fact) that are not what 05 §1 specifies.

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
const NEAR: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
const FAR: [f32; 4] = [0.0, 1.0, 0.0, 0.0];
const HOT: &str = "the cooled hot claim";
const LAST_LOW: &str = "low trust claim 4";

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

fn fact(store: &Store, statement: &str, trust: f64, cooled_until: Option<Timestamp>) {
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
        embedding: Some(Embedding::new(NEAR.to_vec()).expect("valid")),
        embedder_model: None,
        extraction: None,
        cooled_until,
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

/// The trust re-rank corpus shape, with a cooling stamp on the climber: five
/// lower-trust peers created first at distinct descending trusts (0.50 … 0.10, so
/// the competition rank spreads them and they hold the better base ranks), then the
/// high-trust fact created last (worst base rank — only trust can lift it), carrying
/// a cooling window through Jan 8. With `cooling_factor = 0.1`, effective trust
/// inside the window is `0.95 × 0.1 = 0.095 < 0.10`: the climber ranks below even
/// the lowest peer, so whether it climbs over `LAST_LOW` is exactly whether the
/// modulation applied.
fn corpus() -> Arc<Store> {
    let store = store();
    for i in 0..5 {
        fact(
            &store,
            &format!("low trust claim {i}"),
            0.50 - 0.10 * f64::from(i),
            None,
        );
    }
    fact(&store, HOT, 0.95, Some(ts("2026-01-08T00:00:00Z[UTC]")));
    store
}

fn retriever(store: Arc<Store>, cooling_enabled: bool) -> HybridRetriever<FakeEmbedder> {
    HybridRetriever::new(
        store,
        FakeEmbedder::new(),
        RetrieverConfig {
            cooling_enabled,
            cooling_factor: 0.1,
            ..RetrieverConfig::default()
        },
    )
}

async fn recall(r: &HybridRetriever<FakeEmbedder>, now: Option<&str>) -> RecallBundle {
    r.recall(RecallQuery {
        text: QUERY.to_string(),
        principal: Principal::agent(Id::generate()),
        limit: 20,
        options: RecallOptions {
            mode_override: Some(QueryClass::SingleHopFactual),
            fanout: 20,
            now: now.map(ts),
            ..RecallOptions::default()
        },
    })
    .await
    .expect("recall")
}

/// The position the *trust signal* gave this fact — the modulation's direct output.
fn trust_rank(bundle: &RecallBundle, statement: &str) -> usize {
    bundle
        .structured
        .iter()
        .find_map(|e| match e {
            StructuredEntry::Fact(f) if f.statement == statement => Some(
                f.contributions
                    .iter()
                    .find(|c| c.signal == Signal::Trust)
                    .unwrap_or_else(|| panic!("{statement} has a trust contribution"))
                    .rank,
            ),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{statement} surfaced"))
}

#[tokio::test]
async fn a_cooled_fact_is_sunk_inside_its_window_and_recovers_after_it() {
    let store = corpus();
    let r = retriever(Arc::clone(&store), true);

    // Inside the window: effective trust 0.95 x 0.1 = 0.095 < 0.10, so trust ranks
    // the cooled fact below every peer, the lowest included.
    let during = recall(&r, Some("2026-01-03T00:00:00Z[UTC]")).await;
    assert!(
        trust_rank(&during, HOT) > trust_rank(&during, LAST_LOW),
        "inside the window the cooled fact sinks below even the lowest-trust peer"
    );

    // After the window: the comparison stops applying — no write happened, the
    // stamp is still on the row — and stored trust 0.95 ranks it first again.
    let after = recall(&r, Some("2026-01-20T00:00:00Z[UTC]")).await;
    assert_eq!(
        trust_rank(&after, HOT),
        0,
        "the modulation expires without a write"
    );
}

#[tokio::test]
async fn the_modulation_is_double_gated_like_decay() {
    let store = corpus();

    // Cooling enabled but no caller clock: recall ranks by stored trust alone —
    // there is no ambient clock in the retrieval path.
    let r = retriever(Arc::clone(&store), true);
    let unstamped = recall(&r, None).await;
    assert_eq!(
        trust_rank(&unstamped, HOT),
        0,
        "no ambient clock: an unstamped recall never cools"
    );

    // Clock stamped but the policy is off: same.
    let r = retriever(Arc::clone(&store), false);
    let disabled = recall(&r, Some("2026-01-03T00:00:00Z[UTC]")).await;
    assert_eq!(
        trust_rank(&disabled, HOT),
        0,
        "the policy switch alone gates the modulation"
    );
}
