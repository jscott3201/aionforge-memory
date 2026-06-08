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
//! that dilutes a precise answer). Because graph expansion is a per-class on/off decision, the
//! fixture needs only two physical recalls — graph-off (the single-hop profile = the M1
//! capability: lexical + dense + RRF, no associative spread) and graph-on (the multi-hop
//! profile, which adds Personalized PageRank) — scored two ways:
//!
//! - **multi-hop gain:** the router selects graph-on for a multi-hop query, and bridge recall
//!   rises from none (the graph-off baseline) to full recovery (graph-on).
//! - **single-hop no-regression:** the router selects graph-off for a single-hop query; that
//!   path carries no associative contamination, so single-hop precision matches the M1
//!   baseline and cannot regress.
//! - **suppression is load-bearing:** forcing graph-on onto the single-hop query (the
//!   anti-pattern the router avoids) regresses precision, so the suppression is no no-op.
//!
//! The test pins the router's actual per-class decision (via `profile_for`), so the comparison
//! reflects the router's real behavior rather than an assumption about it.
//!
//! Hermetic: a fake embedder maps the query and the near records to one fixed vector and the
//! bridges to an orthogonal one; near, graph-unreachable filler facts fill the dense fan-out
//! so the far bridges sit outside dense and lexical recall and only graph expansion can reach
//! them. The metric therefore measures the conditioning decision, not a wide fan-out sweep.

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
    StructuredEntry, TemporalMode, profile_for,
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
        identity: identity(id),
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
        identity: identity(id),
        stats: stats(),
        subject_id: *subject,
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
    .bind_uuid("from", from)
    .unwrap()
    .bind_uuid("to", to)
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

    // Near filler facts about a disconnected, far entity: their NEAR vectors crowd the dense
    // fact ranking while the FAR subject keeps them off any PageRank reach from `acme`, so they
    // pad dense recall without ever surfacing as graph hits.
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
/// fan-out the metric needs. The class override selects the profile: `SingleHopFactual`
/// suppresses graph expansion (the M1-capability, graph-off path) while `MultiHop` enables it.
async fn recall(r: &HybridRetriever<FakeEmbedder>, class: QueryClass) -> RecallBundle {
    r.recall(RecallQuery {
        text: QUERY.to_string(),
        principal: Principal::agent(Id::generate()),
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

/// Single-hop precision over this fixture: a precise factual recall should carry no
/// associative-only records, so precision is one minus the fraction of graph-only bridge facts
/// (the only off-target records the fixture contains) that leaked into the result.
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

    // The router decision under test: graph expansion is enabled for the multi-hop class and
    // suppressed for the single-hop factual class. The suppression is a `graph_expansion` gate
    // (the PageRank signal does not run), not a zero weight. Pinning it here makes the
    // comparison below a check of the router's real choice, not an assumption about it.
    assert!(
        profile_for(QueryClass::MultiHop).graph_expansion,
        "the router enables graph expansion for the multi-hop class",
    );
    assert!(
        !profile_for(QueryClass::SingleHopFactual).graph_expansion,
        "the router suppresses graph expansion for the single-hop factual class",
    );

    // Two physical recalls over one fixture, read two ways. The graph-off recall is the M1
    // capability (lexical + dense + RRF, no associative spread); the graph-on recall adds
    // Personalized PageRank. A graph-only bridge fact is *signal* for a multi-hop query and
    // *noise* for a single-hop factual query, so each recall scores both metrics.
    let graph_off = recall(&r, QueryClass::SingleHopFactual).await;
    let graph_on = recall(&r, QueryClass::MultiHop).await;

    let mh_recall_off = multi_hop_recall(&graph_off, bridges);
    let mh_recall_on = multi_hop_recall(&graph_on, bridges);
    let sh_precision_off = single_hop_precision(&graph_off, bridges);
    let sh_precision_on = single_hop_precision(&graph_on, bridges);

    // The tracked metric — recorded in the build log so a regression is visible, not silent.
    // Read down the column the router actually selects: multi-hop -> graph-on, single-hop ->
    // graph-off.
    println!("--- M3.T03 conditioning validation ({BRIDGES} graph-only bridge facts) ---");
    println!("                     graph-off (M1)   graph-on");
    println!("multi-hop  recall         {mh_recall_off:.2}           {mh_recall_on:.2}");
    println!("single-hop precision      {sh_precision_off:.2}           {sh_precision_on:.2}");

    // Sanity: the precise single-hop answer is recalled either way — the comparison is about
    // associative spread, not whether the answer is found at all.
    assert!(
        has_fact(&graph_off, ANSWER_FACT) && has_fact(&graph_on, ANSWER_FACT),
        "the single-hop answer is recalled regardless of graph expansion",
    );

    // Multi-hop gain — the router selects graph-on here: graph expansion recovers every
    // graph-only bridge the M1 baseline misses entirely.
    assert_eq!(
        mh_recall_off, 0.0,
        "the M1 baseline (graph off) reaches no graph-only bridge fact",
    );
    assert_eq!(
        mh_recall_on, 1.0,
        "graph expansion recovers all graph-only bridges for the multi-hop class",
    );

    // No single-hop regression — the router selects graph-off here: that path carries no
    // associative-only contamination, so it is already maximally precise and conditioning
    // cannot make single-hop worse than the M1 baseline.
    assert_eq!(
        sh_precision_off, 1.0,
        "the single-hop path the router selects carries no associative-only contamination",
    );

    // The suppression is load-bearing, not vacuous: applying graph expansion to a single-hop
    // query (the anti-pattern the router avoids) pulls off-target associative facts into the
    // result and regresses precision below the baseline. Conditioning is what prevents this.
    assert!(
        sh_precision_on < sh_precision_off,
        "forcing graph expansion on a single-hop query regresses the precision conditioning \
         protects (graph-on={sh_precision_on:.2} < graph-off={sh_precision_off:.2})",
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
