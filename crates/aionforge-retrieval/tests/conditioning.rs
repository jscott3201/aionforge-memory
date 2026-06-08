//! Conditioning validation for graph-augmented retrieval (M3.T03, 03 §3).
//!
//! M3.T01/T02 proved the *mechanics* of graph expansion as booleans: a record reachable
//! only through the associative graph surfaces under the multi-hop class and stays hidden
//! under the single-hop class. M3.T03 turns that same mechanic into a measured, three-way
//! comparison that pins the router's central promise — **expansion helps the classes it
//! should and is suppressed where it hurts** — as a tracked metric, so a future change that
//! quietly regresses either half fails the build.
//!
//! One fixture carries both halves of the claim. A graph-only "bridge" fact is *signal* for
//! an associative/multi-hop query (it is the kind of far, structurally-linked evidence the
//! user wants) and *noise* for a single-hop factual query (an off-target associative fact
//! that dilutes a precise answer). The test scores three conditions over that fixture:
//!
//! - **baseline** — graph expansion off for every query (the M1 capability: lexical + dense
//!   + RRF, no associative spread), obtained by routing through the single-hop profile;
//! - **conditioned** — the router's actual per-class profile (graph on for multi-hop, off
//!   for single-hop);
//! - **mis-conditioned** — graph forced on for the single-hop query (the anti-pattern the
//!   router exists to avoid).
//!
//! and asserts: multi-hop recall rises from baseline to conditioned (gain); single-hop
//! precision is unchanged from baseline to conditioned (no regression); and single-hop
//! precision falls under mis-conditioning (so the suppression is load-bearing, not vacuous).
//!
//! Hermetic: a fake embedder maps the query and the near records to one fixed vector and the
//! bridges to an orthogonal one; near, graph-unreachable filler facts fill the dense fan-out
//! so the far bridges sit outside dense and lexical recall and only graph expansion can reach
//! them. The metric therefore measures the conditioning decision, not a wide fan-out sweep.

use std::future::Future;
use std::sync::Arc;

use aionforge_domain::Retriever;
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
    StructuredEntry, TemporalMode,
};
use aionforge_store::{BoundQuery, NodeId, Store, StoreConfig, Value};

const T0: &str = "2026-01-01T00:00:00Z[UTC]";
const QUERY: &str = "acme";
/// The single-hop factual answer: a fact about the seed entity, near the query in vector
/// space, surfaced by dense recall regardless of graph expansion.
const ANSWER_FACT: &str = "acme is the primary subject";

/// The query vector, and the vector of every near record (answer + dense fillers).
const NEAR: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
/// The bridge vector — orthogonal to the query, so a bridge fact shares no dense similarity
/// and (with no token overlap) no lexical match with the query: only the graph reaches it.
const FAR: [f32; 4] = [0.0, 1.0, 0.0, 0.0];

/// Graph-only bridge facts: each is far in vector space and linked to the answer by a
/// `SUPPORTS` hop, so Personalized PageRank seeded on the query entity reaches them while
/// dense and lexical recall do not. Multi-hop *signal*; single-hop *noise*.
const BRIDGES: usize = 3;
/// Near, graph-unreachable filler facts (about a disconnected entity) that fill the dense
/// fan-out so the far bridges fall outside dense recall — the same crowding the M3.T01 suite
/// uses to isolate the graph signal.
const FILLER_FACTS: usize = 10;
/// Filler entities at the query vector, enough to fill the entity vector seed search so the
/// far bridge-bearing entities never seed the walk; only the lexically-matched `acme` does.
const FILLER_ENTITIES: usize = 5;
/// The recall window and per-signal fan-out, kept equal and wide enough to hold the answer,
/// the dense fillers, and every surfaced bridge so the metric is not truncated by the window.
const WINDOW: usize = 16;

// --- Fake embedder ---------------------------------------------------------------

#[derive(Clone)]
struct FakeEmbedder {
    model: EmbedderModel,
    query_vectors: Vec<(String, [f32; 4])>,
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
        let result = Ok(inputs
            .iter()
            .map(|input| {
                let v = self
                    .query_vectors
                    .iter()
                    .find(|(q, _)| q == input)
                    .map(|(_, v)| v.to_vec())
                    .unwrap_or_else(|| NEAR.to_vec());
                Embedding::new(v).expect("valid fake embedding")
            })
            .collect());
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

fn identity(id: Id) -> Identity {
    Identity {
        id,
        ingested_at: ts(T0),
        namespace: Namespace::Global,
        expired_at: None,
    }
}

fn entity(store: &Store, name: &str, embedding: [f32; 4]) -> (Id, NodeId) {
    let id = Id::generate();
    let entity = Entity {
        identity: identity(id.clone()),
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

fn assert_fact(
    store: &Store,
    subject: &Id,
    subject_node: NodeId,
    statement: &str,
    embedding: [f32; 4],
) -> (Id, NodeId) {
    let id = Id::generate();
    let fact = Fact {
        identity: identity(id.clone()),
        stats: stats(),
        subject_id: subject.clone(),
        predicate: "rel".to_string(),
        object: ObjectValue::Text(statement.to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: statement.to_string(),
        embedding: Some(Embedding::new(embedding.to_vec()).expect("valid")),
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
    let node = store
        .assert_fact(&fact, subject_node, &about)
        .expect("assert fact");
    (id, node)
}

/// Wire `Fact -SUPPORTS-> Fact` by domain id (`weight` is `NOT NULL`, bound as a parameter).
fn support(store: &Store, from: &Id, to: &Id) {
    let q = BoundQuery::new(
        "MATCH (a:Fact {id: $from}), (b:Fact {id: $to}) \
         INSERT (a)-[:SUPPORTS {weight: $weight}]->(b)",
    )
    .bind_str("from", from.as_str())
    .unwrap()
    .bind_str("to", to.as_str())
    .unwrap()
    .bind("weight", Value::Float(1.0))
    .unwrap();
    store.execute(&q).expect("insert SUPPORTS");
}

/// The conditioning fixture and the statements the metric tracks.
struct Fixture {
    store: Arc<Store>,
    /// The graph-only bridge facts: multi-hop gold, single-hop contamination canaries.
    bridges: Vec<String>,
}

/// Build the shared corpus: the seed entity `acme` with one near answer fact; `BRIDGES` far
/// facts each linked to the answer by a `SUPPORTS` hop (graph-only); near, graph-unreachable
/// filler facts that fill the dense fan-out; and filler entities that fill the entity seed
/// search. So a bridge surfaces only when graph expansion runs.
fn fixture() -> Fixture {
    let store = store();
    let (acme, acme_node) = entity(&store, QUERY, NEAR);
    let (answer, _) = assert_fact(&store, &acme, acme_node, ANSWER_FACT, NEAR);

    let mut bridges = Vec::with_capacity(BRIDGES);
    for n in 0..BRIDGES {
        let statement = format!("beta {n} downstream detail");
        let (beta, beta_node) = entity(&store, &format!("beta{n}"), FAR);
        let (bridge, _) = assert_fact(&store, &beta, beta_node, &statement, FAR);
        // answer -SUPPORTS-> bridge: PageRank seeded on `acme` reaches the bridge over the
        // answer; dense (far) and lexical (no shared token) never do.
        support(&store, &answer, &bridge);
        bridges.push(statement);
    }

    // Near, graph-unreachable filler facts about a disconnected entity crowd the dense fact
    // ranking so the far bridges fall outside the dense fan-out.
    let (filler_subject, filler_node) = entity(&store, "filler-subject", FAR);
    for n in 0..FILLER_FACTS {
        assert_fact(
            &store,
            &filler_subject,
            filler_node,
            &format!("unrelated filler {n}"),
            NEAR,
        );
    }
    for n in 0..FILLER_ENTITIES {
        entity(&store, &format!("filler{n}"), NEAR);
    }

    Fixture { store, bridges }
}

fn retriever(store: Arc<Store>) -> HybridRetriever<FakeEmbedder> {
    HybridRetriever::new(
        store,
        FakeEmbedder::new(&[(QUERY, NEAR)]),
        RetrieverConfig::default(),
    )
}

/// Recall the shared query under an explicit class, in Current mode, with the window and
/// fan-out the metric needs. The class override is how the three conditions are expressed:
/// `SingleHopFactual` suppresses graph expansion (baseline / conditioned-single-hop) and
/// `MultiHop` enables it (conditioned-multi-hop / mis-conditioned-single-hop).
async fn recall(r: &HybridRetriever<FakeEmbedder>, class: QueryClass) -> RecallBundle {
    r.recall(RecallQuery {
        text: QUERY.to_string(),
        viewer: Namespace::Global,
        limit: WINDOW,
        options: RecallOptions {
            mode_override: Some(class),
            temporal: TemporalMode::Current,
            fanout: WINDOW,
            ..RecallOptions::default()
        },
    })
    .await
    .expect("recall")
}

fn has_fact(bundle: &RecallBundle, statement: &str) -> bool {
    bundle
        .structured
        .iter()
        .any(|e| matches!(e, StructuredEntry::Fact(f) if f.statement == statement))
}

/// Fraction of the bridge facts present in the bundle.
fn bridge_fraction(bundle: &RecallBundle, bridges: &[String]) -> f64 {
    let hit = bridges.iter().filter(|s| has_fact(bundle, s)).count();
    hit as f64 / bridges.len() as f64
}

/// Multi-hop recall: how much of the graph-only evidence the bridges represent is surfaced.
fn multi_hop_recall(bundle: &RecallBundle, bridges: &[String]) -> f64 {
    bridge_fraction(bundle, bridges)
}

/// Single-hop precision: a precise factual recall should carry no associative-only records,
/// so precision is one minus the fraction of bridge (off-target, graph-only) facts that
/// leaked into the result.
fn single_hop_precision(bundle: &RecallBundle, bridges: &[String]) -> f64 {
    1.0 - bridge_fraction(bundle, bridges)
}

// --- The conditioning metric -----------------------------------------------------

// The metric table is the deliverable: it is emitted to the build log so the conditioning
// numbers are visible on every run, not just the pass/fail of the assertions below. The
// `print_stdout` lint is workspace-warn for exactly this kind of intentional output.
#[tokio::test]
#[allow(clippy::print_stdout)]
async fn conditioning_helps_multi_hop_without_regressing_single_hop() {
    let fx = fixture();
    let r = retriever(fx.store);
    let bridges = &fx.bridges;

    // Baseline (graph off everywhere = the M1 capability) is the single-hop profile applied
    // to the multi-hop intent; conditioned multi-hop is the router's multi-hop profile.
    let baseline_mh = recall(&r, QueryClass::SingleHopFactual).await;
    let conditioned_mh = recall(&r, QueryClass::MultiHop).await;
    // For the single-hop intent, conditioned == the suppressed single-hop profile (which is
    // the baseline), and mis-conditioned forces graph expansion on where the router suppresses it.
    let baseline_sh = recall(&r, QueryClass::SingleHopFactual).await;
    let conditioned_sh = recall(&r, QueryClass::SingleHopFactual).await;
    let misconditioned_sh = recall(&r, QueryClass::MultiHop).await;

    let mh_recall_baseline = multi_hop_recall(&baseline_mh, bridges);
    let mh_recall_conditioned = multi_hop_recall(&conditioned_mh, bridges);
    let sh_precision_baseline = single_hop_precision(&baseline_sh, bridges);
    let sh_precision_conditioned = single_hop_precision(&conditioned_sh, bridges);
    let sh_precision_misconditioned = single_hop_precision(&misconditioned_sh, bridges);

    // The tracked metric — recorded in the build log so a regression is visible, not silent.
    println!("--- M3.T03 conditioning validation ({BRIDGES} graph-only bridge facts) ---");
    println!(
        "multi-hop  recall    baseline={mh_recall_baseline:.2}  conditioned={mh_recall_conditioned:.2}"
    );
    println!(
        "single-hop precision baseline={sh_precision_baseline:.2}  conditioned={sh_precision_conditioned:.2}  mis-conditioned={sh_precision_misconditioned:.2}"
    );

    // Sanity: the single-hop answer is recalled in every condition — the comparison is about
    // associative spread, not whether the precise answer is found at all.
    assert!(
        has_fact(&baseline_sh, ANSWER_FACT) && has_fact(&misconditioned_sh, ANSWER_FACT),
        "the single-hop answer is recalled regardless of graph conditioning",
    );

    // Multi-hop gain: with graph expansion off, none of the graph-only bridges surface;
    // conditioning (graph on for the multi-hop class) recovers them.
    assert_eq!(
        mh_recall_baseline, 0.0,
        "the M1 baseline reaches no graph-only bridge fact",
    );
    assert!(
        mh_recall_conditioned > mh_recall_baseline,
        "multi-hop conditioning surfaces graph-only evidence the baseline misses \
         (conditioned={mh_recall_conditioned:.2} > baseline={mh_recall_baseline:.2})",
    );

    // No single-hop regression: the conditioned single-hop path matches the baseline exactly —
    // a precise factual recall with no associative contamination.
    assert_eq!(
        sh_precision_conditioned, 1.0,
        "the conditioned single-hop recall carries no associative-only contamination",
    );
    assert_eq!(
        sh_precision_conditioned, sh_precision_baseline,
        "single-hop precision is unchanged from the M1 baseline under conditioning",
    );

    // The suppression is load-bearing: forcing graph expansion on for the single-hop query
    // (the anti-pattern conditioning avoids) pulls off-target associative facts into the
    // result and lowers precision below the baseline.
    assert!(
        sh_precision_misconditioned < sh_precision_baseline,
        "mis-conditioned single-hop recall regresses precision the router protects \
         (mis-conditioned={sh_precision_misconditioned:.2} < baseline={sh_precision_baseline:.2})",
    );
}

#[tokio::test]
async fn the_conditioning_comparison_is_deterministic() {
    // The whole comparison rides on byte-identical recall, so each condition must reproduce
    // exactly across calls — otherwise the metric would drift between builds.
    let fx = fixture();
    let r = retriever(fx.store);

    for class in [QueryClass::SingleHopFactual, QueryClass::MultiHop] {
        let first = recall(&r, class).await;
        let second = recall(&r, class).await;
        assert_eq!(
            first.rendered, second.rendered,
            "recall under {class:?} is byte-identical across calls",
        );
    }
}
