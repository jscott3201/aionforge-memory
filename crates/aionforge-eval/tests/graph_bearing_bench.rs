//! Graph-bearing benchmark: measure the marginal lift of the global-authority signal (R1) and
//! the community-diversity cap (R2) on a store that has facts + entities + SUPPORTS edges.
//!
//! The earlier eval runners are episode-only — the deterministic extractor derives no facts from
//! BEAM's narrative prose, so the `{Entity,Fact,Episode}×{MENTIONS,ABOUT,SUPPORTS}` projection that
//! authority and communities run over is empty. This runner builds a graph-bearing store the
//! supported way: a clean subject-verb-object corpus (the 4 markers the rule extractor fires on:
//! `works on`/`is based in`/`prefers`/`uses`) is seeded as raw episodes and run through the REAL
//! consolidation pipeline in-process, producing facts, resolved entities, `ABOUT`/`MENTIONS` edges,
//! and `Episode -SUPPORTS-> Fact` edges. Authority and the cap then have a real graph to act on.
//!
//! Honesty notes:
//! - The corpus is synthetic, so this validates the MECHANISM + calibrates a value on a controlled
//!   graph (where authority correlates with relevance and one cluster can dominate). Real-world
//!   transfer is a separate question (needs M4 model-backed extraction over real prose).
//! - The dense floor is disabled (`min_relevance = Some(0.0)`) for the sweeps so a dense-far gold is
//!   not floored out before the ranking signals can act — the floor is a separate (off-topic) concern.
//! - A deterministic fake embedder (topic-vector overrides + a hash fallback) gives full control of
//!   dense-vs-graph recovery with no network, and distinct entity surfaces stay near-orthogonal so
//!   consolidation never wrongly merges them.
//!
//! Run: `cargo test -p aionforge-eval --test graph_bearing_bench -- --ignored --nocapture`
//! The `the_svo_corpus_consolidates_into_a_graph` guard is NOT ignored — it fails loudly if the
//! consolidation ever stops producing a graph (so a silently-empty projection can't pass as "0 lift").

// This runner's output IS its deliverable: a human reads the printed sweep tables. The workspace
// keeps print_stdout at warn for exactly such cases; allow it here.
#![allow(clippy::print_stdout)]

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use aionforge_consolidate::{
    ConsolidationConfig, Consolidator, FactExtractionPass, PassConfig, RuleExtractor,
    RuleSummarizer,
};
use aionforge_domain::Retriever;
use aionforge_domain::authz::Principal;
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::time::Timestamp;
use aionforge_eval::{community_redundancy, ndcg_at_k, ranked_ids, recall_at_k};
use aionforge_retrieval::{
    HybridRetriever, QueryClass, RecallBundle, RecallOptions, RecallQuery, RetrieverConfig,
    TemporalMode,
};
use aionforge_store::{BoundQuery, QueryResult, Store, StoreConfig, Value};

const T0: &str = "2026-01-01T00:00:00Z[UTC]";
const DIM: usize = 16;

// --- Deterministic fake embedder --------------------------------------------------

/// A topic basis vector `e_topic` (unit on one axis) — used to put a query and its candidate
/// facts at the SAME dense position, so the only thing separating them is the graph (authority /
/// community), not vector similarity.
fn topic(axis: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    v[axis % DIM] = 1.0;
    v
}

/// A deterministic near-orthogonal unit vector from a string (FNV-1a seed + xorshift fill). Distinct
/// texts land far apart, so entity surfaces never wrongly merge and an un-mapped fact is dense-far.
fn hash_vec(text: &str) -> Vec<f32> {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut x = h | 1;
    let mut v = Vec::with_capacity(DIM);
    for _ in 0..DIM {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let unit = (x >> 11) as f64 / (1u64 << 53) as f64;
        v.push((unit * 2.0 - 1.0) as f32);
    }
    let norm = v
        .iter()
        .map(|c| c * c)
        .sum::<f32>()
        .sqrt()
        .max(f32::EPSILON);
    v.iter().map(|c| c / norm).collect()
}

#[derive(Clone)]
struct FakeEmbedder {
    model: EmbedderModel,
    /// Exact-text overrides: queries and candidate fact statements assigned to a topic vector.
    overrides: HashMap<String, Vec<f32>>,
}

impl FakeEmbedder {
    fn new(overrides: HashMap<String, Vec<f32>>) -> Self {
        Self {
            model: EmbedderModel {
                family: "fake-graph-bench".to_string(),
                version: "1".to_string(),
                dimension: DIM as u32,
            },
            overrides,
        }
    }

    fn vector(&self, text: &str) -> Vec<f32> {
        self.overrides
            .get(text)
            .cloned()
            .unwrap_or_else(|| hash_vec(text))
    }
}

#[derive(Debug)]
struct FakeEmbedError;
impl std::fmt::Display for FakeEmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("fake embedder never fails")
    }
}
impl std::error::Error for FakeEmbedError {}

impl Embedder for FakeEmbedder {
    type Error = FakeEmbedError;
    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let out: Vec<Embedding> = inputs
            .iter()
            .map(|t| Embedding::new(self.vector(t)).expect("valid fake embedding"))
            .collect();
        async move { Ok(out) }
    }
    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

// --- Store / consolidation helpers -------------------------------------------------

fn ts() -> Timestamp {
    T0.parse().expect("valid timestamp")
}

fn store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: DIM as u32,
    })
    .expect("open store");
    store.migrate(&ts()).expect("migrate store");
    Arc::new(store)
}

fn raw_episode(store: &Store, content: &str) {
    let id = Id::generate();
    let episode = Episode {
        identity: Identity {
            id,
            ingested_at: ts(),
            namespace: Namespace::Global,
            expired_at: None,
        },
        stats: Stats {
            importance: 0.5,
            trust: 0.8,
            last_access: ts(),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.1,
            is_pinned: false,
        },
        content: content.to_string(),
        role: Role::User,
        captured_at: ts(),
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: None, // raw: consolidation embeds the derived facts via the fake embedder
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("insert raw episode");
}

/// Seed the SVO episodes and run the real consolidation pipeline to a fixed point, returning the
/// graph-bearing store.
async fn consolidate(store: &Arc<Store>, embedder: FakeEmbedder) {
    let mut c = Consolidator::new(Arc::clone(store), ConsolidationConfig::default());
    c.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(embedder),
        Arc::new(RuleSummarizer::with_default_rules()),
        PassConfig::default(),
    )));
    loop {
        let report = c.tick_once().await.expect("consolidation tick");
        if report.pending_after == 0 {
            break;
        }
    }
}

fn count(store: &Store, query: &str) -> usize {
    let QueryResult::Rows(rows) = store.execute(&BoundQuery::new(query)).expect("count query")
    else {
        return 0;
    };
    rows.row_count()
}

/// The largest number of facts pointing `ABOUT` any single entity — the in-degree of the most
/// connected hub. Counted Rust-side (one row per Fact->Entity ABOUT edge) to avoid leaning on GQL
/// aggregation support.
fn max_about_in_degree(store: &Store) -> usize {
    let q = BoundQuery::new("MATCH (f:Fact)-[:ABOUT]->(e:Entity) RETURN e.id AS id");
    let QueryResult::Rows(rows) = store.execute(&q).expect("about query") else {
        return 0;
    };
    let idx = rows.column_index("id").expect("id column");
    let mut counts: HashMap<Id, usize> = HashMap::new();
    for r in 0..rows.row_count() {
        if let Some(Value::Uuid(u)) = rows.value(r, idx) {
            *counts.entry(Id::from_uuid(*u)).or_insert(0) += 1;
        }
    }
    counts.values().copied().max().unwrap_or(0)
}

/// The domain id of the `Fact` whose statement equals `statement` (the SVO sentence). Panics if the
/// statement did not consolidate into exactly one fact — a loud signal the corpus drifted from what
/// the extractor supports.
fn fact_id(store: &Store, statement: &str) -> Id {
    let q = BoundQuery::new("MATCH (f:Fact {statement: $s}) RETURN f.id AS id")
        .bind_str("s", statement)
        .expect("bind statement");
    let QueryResult::Rows(rows) = store.execute(&q).expect("fact lookup") else {
        panic!("fact lookup returned no rows table");
    };
    assert_eq!(
        rows.row_count(),
        1,
        "expected exactly one fact for statement {statement:?}, got {}",
        rows.row_count()
    );
    let idx = rows.column_index("id").expect("id column");
    match rows.value(0, idx).expect("id value") {
        Value::Uuid(u) => Id::from_uuid(*u),
        other => panic!("unexpected id value {other:?}"),
    }
}

// --- Corpus ------------------------------------------------------------------------

/// SVO episodes. Two structures: an R1 "authority" set on topic 0 (a hub entity `Aionforge` that
/// four people work on, vs four peripheral one-off projects — all at the same dense position, so
/// authority is the only differentiator), and an R2 "community" set on topic 2 (a dominant `Quinn`
/// cluster of four facts vs two lone diverse facts). The two sets use disjoint entities/topics so a
/// fact has one unambiguous dense vector and one unambiguous authored cluster.
///
/// R2 topology note: the four dominant facts are seeded as ONE multi-sentence episode (see
/// [`graph_store`]). That makes them share both their subject (`ABOUT` -> Quinn) AND their source
/// episode (`SUPPORTS`), so native Louvain groups them into a single community — the prerequisite
/// for the diversity cap to bite. Were each fact its own episode (with its own private object
/// entity), the `{fact, episode, object}` triangles fragment into singleton communities and the
/// cap has nothing to group (a real constraint on R2: it caps associative communities, not
/// same-subject facts).
const R1_HUB_GOLD: [&str; 4] = [
    "Alice works on Aionforge",
    "Bob works on Aionforge",
    "Carol works on Aionforge",
    "Dave works on Aionforge",
];
const R1_PERIPHERAL: [&str; 4] = [
    "Eve works on Trinket",
    "Frank works on Gadget",
    "Grace works on Widget",
    "Heidi works on Sprocket",
];
const R1_CONNECTED_QUERY: &str = "connected project hub";
const R1_CONNECTED_HUB_GOLD: [&str; 4] = [
    "Aionforge uses Rust",
    "Aionforge uses Selene",
    "Aionforge prefers Determinism",
    "Aionforge is based in Boston",
];
const R1_CONNECTED_PERIPHERAL: [&str; 4] = [
    "Trinket uses Ruby",
    "Gadget prefers Sqlite",
    "Widget is based in Denver",
    "Sprocket works on Glue",
];
const R2_DOMINANT: [&str; 4] = [
    "Quinn works on Beacon",
    "Quinn uses Postgres",
    "Quinn prefers Vim",
    "Quinn is based in Boston",
];
const R2_DIVERSE: [&str; 2] = ["Rosa works on Lantern", "Sam works on Harbor"];

/// The embedder override table — queries and their candidate fact statements pinned to topic
/// vectors so dense is INDIFFERENT within a query's candidate set (and the query lands on its
/// candidates). The SAME table feeds the consolidation embedder (which embeds the derived fact
/// statements) and the retrieval embedder (which embeds the query) — otherwise the two disagree on
/// where a text sits and the query never lands near its facts.
fn bench_overrides() -> HashMap<String, Vec<f32>> {
    let mut overrides: HashMap<String, Vec<f32>> = HashMap::new();
    // R1 query + its 8 candidates at topic 0 (dense-equal; authority differentiates).
    overrides.insert("aionforge contributors".to_string(), topic(0));
    for s in R1_HUB_GOLD.iter().chain(R1_PERIPHERAL.iter()) {
        overrides.insert((*s).to_string(), topic(0));
    }
    // R2 query + its candidates at topic 2; the diverse facts a hair off so the dominant cluster
    // out-ranks them without the cap (mirrors the community_diversity integration fixture).
    overrides.insert("project work".to_string(), topic(2));
    for s in R2_DOMINANT {
        overrides.insert(s.to_string(), topic(2));
    }
    let mut nearish = topic(2);
    nearish[3] = 0.15; // cosine ~0.99 with topic(2): retrievable, just below the dominant cluster
    for s in R2_DIVERSE {
        overrides.insert(s.to_string(), nearish.clone());
    }
    overrides
}

/// Connected R1 overrides: every candidate is dense-equal to the query, so any lift comes from the
/// connected authority topology rather than vector distance.
fn connected_r1_overrides() -> HashMap<String, Vec<f32>> {
    let mut overrides: HashMap<String, Vec<f32>> = HashMap::new();
    overrides.insert(R1_CONNECTED_QUERY.to_string(), topic(4));
    for s in R1_CONNECTED_HUB_GOLD
        .iter()
        .chain(R1_CONNECTED_PERIPHERAL.iter())
    {
        overrides.insert((*s).to_string(), topic(4));
    }
    overrides
}

async fn graph_store_from(
    episodes: impl IntoIterator<Item = String>,
    overrides: HashMap<String, Vec<f32>>,
) -> Arc<Store> {
    let store = store();
    for episode in episodes {
        raw_episode(&store, &episode);
    }
    consolidate(&store, FakeEmbedder::new(overrides)).await;
    store
}

/// Build the graph-bearing store: seed the SVO episodes and run the real consolidation pipeline.
///
/// R1 facts + the two diverse R2 facts are each their own one-sentence episode. The four dominant
/// R2 facts are ONE multi-sentence episode so they share a source episode (see the corpus note) —
/// the structure that lets Louvain group them into the single community the cap demotes.
async fn graph_store() -> Arc<Store> {
    let mut episodes: Vec<String> = R1_HUB_GOLD
        .iter()
        .chain(R1_PERIPHERAL.iter())
        .chain(R2_DIVERSE.iter())
        .map(|s| (*s).to_string())
        .collect();
    // The dominant cluster as a single episode: "Quinn works on Beacon. Quinn uses Postgres. ..."
    // splits back into the four period-free sentences (the splitter drops the delimiter), so each
    // derived fact's statement — and thus its dense override key — is unchanged.
    episodes.push(R2_DOMINANT.join(". "));
    graph_store_from(episodes, bench_overrides()).await
}

/// Connected R1 store: every candidate fact shares one source episode, forming one connected
/// component. The gold facts additionally share the subject entity (`Aionforge`), and each
/// hub-gold fact appears in extra raw episodes that dedupe to the same fact but add SUPPORTS
/// degree. That gives global authority connected hub facts to reward rather than isolated islands.
async fn connected_r1_graph_store() -> Arc<Store> {
    let shared_episode = R1_CONNECTED_HUB_GOLD
        .iter()
        .chain(R1_CONNECTED_PERIPHERAL.iter())
        .copied()
        .collect::<Vec<_>>()
        .join(". ");
    let mut episodes = vec![shared_episode];
    for gold in R1_CONNECTED_HUB_GOLD {
        episodes.extend((0..5).map(|_| gold.to_string()));
    }
    graph_store_from(episodes, connected_r1_overrides()).await
}

fn retriever_with_overrides(
    store: Arc<Store>,
    overrides: HashMap<String, Vec<f32>>,
    authority_weight: Option<f64>,
) -> HybridRetriever<FakeEmbedder> {
    HybridRetriever::new(
        store,
        FakeEmbedder::new(overrides),
        RetrieverConfig {
            authority_weight,
            ..RetrieverConfig::default()
        },
    )
}

fn retriever(store: Arc<Store>, authority_weight: Option<f64>) -> HybridRetriever<FakeEmbedder> {
    retriever_with_overrides(store, bench_overrides(), authority_weight)
}

fn connected_r1_retriever(
    store: Arc<Store>,
    authority_weight: Option<f64>,
) -> HybridRetriever<FakeEmbedder> {
    retriever_with_overrides(store, connected_r1_overrides(), authority_weight)
}

async fn recall(
    r: &HybridRetriever<FakeEmbedder>,
    text: &str,
    class: QueryClass,
    temporal: TemporalMode,
    limit: usize,
    community_cap: usize,
) -> RecallBundle {
    r.recall(RecallQuery {
        text: text.to_string(),
        principal: Principal::agent(Id::generate()),
        limit,
        options: RecallOptions {
            mode_override: Some(class),
            // Which bi-temporal slice to read facts against. `Current` runs the full associative
            // stack (graph/support/authority) for the R1 read; `History` reads the whole record
            // with no Current-gated seed/support, isolating the dense ranking for the R2 cap sweep.
            temporal,
            // Disable the dense floor so a dense-far gold is not floored before authority can lift
            // it — the floor is a separate off-topic-rejection concern, measured elsewhere.
            min_relevance: Some(0.0),
            community_diversity_cap: community_cap,
            fanout: 32,
            ..RecallOptions::default()
        },
    })
    .await
    .expect("recall")
}

// --- Guard (NOT ignored): the corpus really builds a graph --------------------------

#[tokio::test]
async fn the_svo_corpus_consolidates_into_a_graph() {
    let store = graph_store().await;
    let facts = count(&store, "MATCH (f:Fact) RETURN f.id AS id");
    let entities = count(&store, "MATCH (e:Entity) RETURN e.id AS id");
    let supports = count(&store, "MATCH ()-[s:SUPPORTS]->() RETURN s");
    // 14 SVO episodes -> 14 facts; the projection authority/communities need is non-empty.
    assert_eq!(facts, 14, "every SVO episode derives one fact");
    assert!(entities > 0, "consolidation resolved entities");
    assert!(
        supports >= 14,
        "each fact gets an Episode -SUPPORTS-> Fact edge, got {supports}"
    );
    // A hub exists: some entity is ABOUT'd by several facts (the authority/community structure).
    // Print the per-entity ABOUT in-degree so a corpus drift is diagnosable, then assert the
    // top hub has >= 4 incident facts (the four `works on Aionforge` / `... Beacon` facts).
    let about = count(&store, "MATCH ()-[a:ABOUT]->() RETURN a");
    let top_hub = max_about_in_degree(&store);
    println!(
        "graph: {facts} facts, {entities} entities, {supports} SUPPORTS, {about} ABOUT; top hub in-degree = {top_hub}"
    );
    assert!(
        top_hub >= 4,
        "expected a >=4-fact hub entity (the shared project), top in-degree was {top_hub}"
    );
}

// --- R1: authority-weight sweep (#[ignore]) ----------------------------------------

#[tokio::test]
#[ignore = "graph-bearing benchmark: run on demand with --ignored --nocapture"]
async fn authority_weight_sweep_reports_marginal_lift() {
    let store = graph_store().await;
    let k = 4;
    let gold: std::collections::HashSet<Id> =
        R1_HUB_GOLD.iter().map(|s| fact_id(&store, s)).collect();
    let grades: HashMap<Id, u8> = gold.iter().map(|id| (*id, 1u8)).collect();

    println!("\n================ R1 — global-authority weight sweep ================");
    println!(
        "query 'aionforge contributors' | {} hub gold + {} peripheral distractors, dense-equal at topic 0",
        R1_HUB_GOLD.len(),
        R1_PERIPHERAL.len()
    );
    println!("weight  recall@{k}  ndcg@{k}");
    println!("------------------------------------");
    for weight in [None, Some(0.3), Some(0.6), Some(1.0), Some(2.0)] {
        let r = retriever(Arc::clone(&store), weight);
        let bundle = recall(
            &r,
            "aionforge contributors",
            QueryClass::MultiHop,
            TemporalMode::Current,
            k,
            0,
        )
        .await;
        let ranked = ranked_ids(&bundle);
        let recall = recall_at_k(&ranked, &gold, k);
        let ndcg = ndcg_at_k(&ranked, &grades, k);
        let label = weight.map_or("off".to_string(), |w| format!("{w:.1}"));
        println!("{label:<6}  {recall:.3}      {ndcg:.3}");
        if weight == Some(2.0) {
            // At the heaviest authority weight, print each fact's per-signal ranks so the topology
            // effect is legible: on this DISCONNECTED graph, undirected PageRank gives peripheral
            // island facts (small components, little teleport competition) a HIGHER authority rank
            // than the hub-connected gold — see the trailing note.
            println!("  signals_run = {:?}", bundle.explanation.signals_run);
            for entry in &bundle.structured {
                if let aionforge_retrieval::StructuredEntry::Fact(f) = entry {
                    let sigs: Vec<_> = f
                        .contributions
                        .iter()
                        .map(|c| format!("{:?}#{}", c.signal, c.rank))
                        .collect();
                    println!("    [{:.4}] {} | {sigs:?}", f.score, f.statement);
                }
            }
        }
    }
    println!(
        "(authority is a GLOBAL, query-independent prior. On a DISCONNECTED projection undirected \
         PageRank concentrates mass in SMALL components, so peripheral island facts can out-rank \
         hub-connected gold — recall stays flat across weights here. Authority's 'hubs win' \
         intuition needs a connected graph; this is a real input for R1-activate.)"
    );
}

#[tokio::test]
#[ignore = "graph-bearing benchmark: run on demand with --ignored --nocapture"]
async fn connected_authority_weight_sweep_reports_marginal_lift() {
    let store = connected_r1_graph_store().await;
    let facts = count(&store, "MATCH (f:Fact) RETURN f.id AS id");
    let top_hub = max_about_in_degree(&store);
    assert_eq!(facts, 8, "connected R1 corpus should derive 8 facts");
    assert!(
        top_hub >= R1_CONNECTED_HUB_GOLD.len(),
        "expected the connected hub to have at least {} ABOUT edges, got {top_hub}",
        R1_CONNECTED_HUB_GOLD.len()
    );

    let k = 4;
    let gold: std::collections::HashSet<Id> = R1_CONNECTED_HUB_GOLD
        .iter()
        .map(|s| fact_id(&store, s))
        .collect();
    let grades: HashMap<Id, u8> = gold.iter().map(|id| (*id, 1u8)).collect();

    println!("\n================ R1 — CONNECTED global-authority sweep ================");
    println!(
        "query '{R1_CONNECTED_QUERY}' | {} hub gold facts share ABOUT->Aionforge + extra SUPPORTS degree; {} peripheral distractors share only the source episode; dense-equal at topic 4",
        R1_CONNECTED_HUB_GOLD.len(),
        R1_CONNECTED_PERIPHERAL.len()
    );
    println!("weight  recall@{k}  ndcg@{k}");
    println!("------------------------------------");
    for weight in [None, Some(0.3), Some(0.6), Some(1.0), Some(2.0)] {
        let r = connected_r1_retriever(Arc::clone(&store), weight);
        let bundle = recall(
            &r,
            R1_CONNECTED_QUERY,
            QueryClass::MultiHop,
            // History keeps MultiHop's Graph + Authority signals active, but removes Current-only
            // Support expansion. That isolates the marginal authority lift on this connected
            // topology; in Current mode Support already recovers the reinforced hub facts.
            TemporalMode::History,
            k,
            0,
        )
        .await;
        let ranked = ranked_ids(&bundle);
        let recall = recall_at_k(&ranked, &gold, k);
        let ndcg = ndcg_at_k(&ranked, &grades, k);
        let label = weight.map_or("off".to_string(), |w| format!("{w:.1}"));
        println!("{label:<6}  {recall:.3}      {ndcg:.3}");
        if weight == Some(2.0) {
            println!("  signals_run = {:?}", bundle.explanation.signals_run);
            for entry in &bundle.structured {
                if let aionforge_retrieval::StructuredEntry::Fact(f) = entry {
                    let sigs: Vec<_> = f
                        .contributions
                        .iter()
                        .map(|c| format!("{:?}#{}", c.signal, c.rank))
                        .collect();
                    println!("    [{:.4}] {} | {sigs:?}", f.score, f.statement);
                }
            }
        }
    }
    println!(
        "(connected case: all candidate facts sit in one component, and the hub-gold facts share \
         the Aionforge subject inside it; the hub-gold facts also carry repeated SUPPORTS, so \
         seedless authority can reward connected structural evidence rather than isolated islands. \
         Compare this with the disconnected table above before choosing any R1 activation weight.)"
    );
}

// --- R2: community-cap sweep (#[ignore]) -------------------------------------------

#[tokio::test]
#[ignore = "graph-bearing benchmark: run on demand with --ignored --nocapture"]
async fn community_cap_sweep_reports_redundancy_vs_recall() {
    let store = graph_store().await;
    let k = 3;
    // Authored clusters by subject entity (the redundancy ground truth): all four dominant facts
    // are the one `quinn` cluster; each diverse fact is its own one-off (`rosa`, `sam`). Labeling
    // the diverse facts distinctly matches their distinct Louvain communities, so the metric
    // credits surfacing two *different* entities as zero redundancy rather than one.
    let subject = |s: &str| s.split_whitespace().next().unwrap_or(s).to_lowercase();
    let mut cluster_of: HashMap<Id, String> = HashMap::new();
    for s in R2_DOMINANT.iter().chain(R2_DIVERSE.iter()) {
        cluster_of.insert(fact_id(&store, s), subject(s));
    }
    // Gold = diverse coverage: we want the bundle to surface the lone diverse facts, not a wall of
    // the dominant cluster.
    let gold: std::collections::HashSet<Id> =
        R2_DIVERSE.iter().map(|s| fact_id(&store, s)).collect();

    println!("\n================ R2 — community-diversity cap sweep ================");
    println!(
        "query 'project work' | dominant {}-fact 'quinn' cluster vs {} diverse facts | limit {k}",
        R2_DOMINANT.len(),
        R2_DIVERSE.len()
    );
    println!("cap   recall@{k}(diverse)  redundancy@{k}");
    println!("--------------------------------------------");
    // Isolate the cap: SingleHopFactual has `graph_expansion = false` (no Graph signal) and
    // `support = OFF`, and reading in `History` (a non-Current slice) bypasses the Current-gated
    // high-precision dense seed + support expansion. The result is a plain global fact dense KNN
    // (Lexical/Dense/Trust) over the whole record — the dense-relevant cluster ranks on top and
    // the diversity cap is the only thing that can break it up, so its effect is measured cleanly.
    let r = retriever(Arc::clone(&store), None);
    for cap in [0usize, 1, 2, 3] {
        let bundle = recall(
            &r,
            "project work",
            QueryClass::SingleHopFactual,
            TemporalMode::History,
            k,
            cap,
        )
        .await;
        let ranked = ranked_ids(&bundle);
        let recall_diverse = recall_at_k(&ranked, &gold, k);
        let redundancy = community_redundancy(&ranked, &cluster_of, k);
        let label = if cap == 0 {
            "off".to_string()
        } else {
            cap.to_string()
        };
        println!("{label:<5} {recall_diverse:.3}             {redundancy:.3}");
        if cap == 0 {
            // Show the uncapped bundle so the pathology is legible: the dominant `quinn` cluster
            // (dense ~1.0) fills the limit and the diverse one-offs are squeezed out.
            println!(
                "  uncapped top-{k} (signals {:?}):",
                bundle.explanation.signals_run
            );
            for entry in &bundle.structured {
                if let aionforge_retrieval::StructuredEntry::Fact(f) = entry {
                    println!(
                        "    [{:.4}] cluster={:<6} {}",
                        f.score,
                        cluster_of.get(&f.id).map_or("?", String::as_str),
                        f.statement,
                    );
                }
            }
        }
    }
    println!(
        "(cap 0 = no constraint: the dominant cluster fills the bundle, high redundancy, diverse \
         gold squeezed out. A cap promotes diversity — watch redundancy fall and diverse recall rise.)"
    );
}
