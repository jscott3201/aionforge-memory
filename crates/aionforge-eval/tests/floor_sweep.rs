//! On-demand off-topic-rejection floor sweep against the REAL embedder.
//!
//! This is a `#[ignore]` integration test, not a CI gate: it makes network calls to the
//! production embedder and is run by hand to pick a responsible per-class `min_relevance`
//! floor. It embeds the labeled fixture exactly once, caches the vectors, seeds a store,
//! and then re-runs recall across a sweep of floor values with ZERO additional network
//! calls (fusion and the floor run after embedding). Output is a `SweepReport` written to
//! a temp file and printed.
//!
//! Run it with:
//!
//! ```bash
//! source ~/.aionforge/aionforge-redeploy.env   # provides AIONFORGE_EMBEDDER_API_KEY
//! cargo test -p aionforge-eval --test floor_sweep -- --ignored --nocapture
//! ```
//!
//! With the key unset it skips with a message (so an accidental CI invocation is a no-op).

// This runner's output IS its deliverable: a human reads the printed sweep table. The
// workspace keeps print_stdout at warn for exactly such legitimate cases; allow it here.
#![allow(clippy::print_stdout)]

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use aionforge_domain::Retriever;
use aionforge_domain::authz::Principal;
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::time::Timestamp;
use aionforge_embed::HttpEmbedder;
use aionforge_eval::{
    FloorReport, MemoryRow, QueryRow, SweepReport, false_rejection_rate, ndcg_at_k, parse_memories,
    parse_queries, ranked_ids, recall_at_k, rejection_rate, scrub_violations,
};
use aionforge_retrieval::{
    HybridRetriever, QueryClass, RecallBundle, RecallOptions, RecallQuery, RetrieverConfig,
    classify,
};
use aionforge_store::{Store, StoreConfig};
use secrecy::SecretString;

const MEMORIES: &str = include_str!("../fixtures/corpus_memories.jsonl");
const QUERIES: &str = include_str!("../fixtures/corpus_queries.jsonl");

const ENDPOINT: &str = "https://openrouter.ai/api/v1";
const MODEL: &str = "google/gemini-embedding-2";
const DIMENSION: u32 = 3072;
const T0: &str = "2026-01-01T00:00:00Z[UTC]";
const KEY_ENV: &str = "AIONFORGE_EMBEDDER_API_KEY";
const PRICE_PER_MTOK_ENV: &str = "AIONFORGE_FLOOR_SWEEP_PRICE_PER_MTOK";
const DEFAULT_PRICE_PER_MTOK: f64 = 0.15;

/// The candidate floors to sweep. `0.0` is the current default (OFF); the calibration note
/// puts on-topic dense similarity ~0.6-0.85 and off-topic below ~0.45, so the interesting
/// separation is around 0.40-0.65.
const SWEEP: [f64; 10] = [0.0, 0.35, 0.40, 0.45, 0.50, 0.55, 0.60, 0.62, 0.65, 0.70];
/// The cutoff for recall@k / nDCG@k.
const K: usize = 5;
/// The false-rejection ceiling used to pick the best floor.
const MAX_FALSE_REJECTION: f64 = 0.0;

#[derive(Debug, Clone, Copy)]
struct QualityMetrics {
    rejection_rate: f64,
    false_rejection_rate: f64,
    recall_at_k: f64,
    ndcg_at_k: f64,
}

impl QualityMetrics {
    fn as_floor_report(self, floor: f64) -> FloorReport {
        FloorReport {
            floor,
            rejection_rate: self.rejection_rate,
            false_rejection_rate: self.false_rejection_rate,
            recall_at_k: self.recall_at_k,
            ndcg_at_k: self.ndcg_at_k,
        }
    }
}

/// An embedder that serves pre-computed vectors from a cache, so a multi-floor sweep
/// re-runs recall with zero network calls. Every text it will be asked for (the query
/// texts) is warmed before the sweep, so a miss is a fixture/runner bug.
struct CachingEmbedder {
    cache: HashMap<String, Embedding>,
    model: EmbedderModel,
}

#[derive(Debug)]
struct CacheMiss(String);

impl std::fmt::Display for CacheMiss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no cached embedding for {:?}", self.0)
    }
}

impl std::error::Error for CacheMiss {}

impl Embedder for CachingEmbedder {
    type Error = CacheMiss;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let result: Result<Vec<Embedding>, CacheMiss> = inputs
            .iter()
            .map(|input| {
                self.cache
                    .get(input)
                    .cloned()
                    .ok_or_else(|| CacheMiss(input.clone()))
            })
            .collect();
        async move { result }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid timestamp")
}

fn seed_store(
    memories: &[MemoryRow],
    cache: &HashMap<String, Embedding>,
) -> (Arc<Store>, HashMap<String, Id>) {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: DIMENSION,
    })
    .expect("open store");
    store.migrate(&ts(T0)).expect("migrate store");

    let mut id_map = HashMap::new();
    for row in memories {
        let id = Id::generate();
        id_map.insert(row.id.clone(), id);
        let episode = Episode {
            identity: Identity {
                id,
                ingested_at: ts(T0),
                namespace: Namespace::Global,
                expired_at: None,
            },
            stats: Stats {
                importance: row.importance,
                trust: row.trust,
                last_access: ts(T0),
                access_count_recent: 0,
                referenced_count: 0,
                surprise: 0.1,
                is_pinned: false,
            },
            content: row.text.clone(),
            role: Role::User,
            captured_at: ts(T0),
            agent_id: Id::generate(),
            session_id: None,
            content_hash: ContentHash::of(row.text.as_bytes()),
            embedding: cache.get(&row.text).cloned(),
            embedder_model: None,
            consolidation_state: ConsolidationState::Raw,
            origin: None,
        };
        store.insert_episode(&episode).expect("insert episode");
    }
    (Arc::new(store), id_map)
}

async fn recall_with_floor(
    retriever: &HybridRetriever<CachingEmbedder>,
    query: &str,
    floor: Option<f64>,
) -> RecallBundle {
    retriever
        .recall(RecallQuery {
            text: query.to_string(),
            principal: Principal::agent(Id::generate()),
            limit: 10,
            options: RecallOptions {
                fanout: 20,
                min_relevance: floor,
                ..RecallOptions::default()
            },
        })
        .await
        .expect("recall")
}

/// Translate a query's fixture-id gold labels into seeded domain ids.
fn gold_for(query: &QueryRow, id_map: &HashMap<String, Id>) -> (HashSet<Id>, HashMap<Id, u8>) {
    let mut gold = HashSet::new();
    let mut grades = HashMap::new();
    for graded in &query.expected {
        if let Some(id) = id_map.get(&graded.id) {
            grades.insert(*id, graded.grade);
            if graded.grade > 0 {
                gold.insert(*id);
            }
        }
    }
    (gold, grades)
}

async fn score_queries(
    retriever: &HybridRetriever<CachingEmbedder>,
    queries: &[QueryRow],
    id_map: &HashMap<String, Id>,
    floor: Option<f64>,
) -> QualityMetrics {
    let mut negatives: Vec<RecallBundle> = Vec::new();
    let mut positives: Vec<(RecallBundle, HashSet<Id>, HashMap<Id, u8>)> = Vec::new();
    for query in queries {
        let bundle = recall_with_floor(retriever, &query.query, floor).await;
        if query.is_negative() {
            negatives.push(bundle);
        } else {
            let (gold, grades) = gold_for(query, id_map);
            positives.push((bundle, gold, grades));
        }
    }

    let neg_refs: Vec<&RecallBundle> = negatives.iter().collect();
    let pos_for_fr: Vec<(&RecallBundle, HashSet<Id>)> =
        positives.iter().map(|(b, g, _)| (b, g.clone())).collect();
    let pos_count = positives.len().max(1) as f64;
    let recall_sum: f64 = positives
        .iter()
        .map(|(b, gold, _)| recall_at_k(&ranked_ids(b), gold, K))
        .sum();
    let ndcg_sum: f64 = positives
        .iter()
        .map(|(b, _, grades)| ndcg_at_k(&ranked_ids(b), grades, K))
        .sum();

    QualityMetrics {
        rejection_rate: rejection_rate(&neg_refs),
        false_rejection_rate: false_rejection_rate(&pos_for_fr),
        recall_at_k: recall_sum / pos_count,
        ndcg_at_k: ndcg_sum / pos_count,
    }
}

fn estimate_tokens(texts: &[String]) -> usize {
    let mut seen = HashSet::new();
    texts
        .iter()
        .filter(|text| seen.insert(text.as_str()))
        .map(|text| text.chars().count().div_ceil(4))
        .sum()
}

fn price_per_mtok() -> f64 {
    std::env::var(PRICE_PER_MTOK_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|price| *price >= 0.0)
        .unwrap_or(DEFAULT_PRICE_PER_MTOK)
}

#[test]
fn fixture_has_broadened_negatives_and_positive_class_coverage() {
    let memories = parse_memories(MEMORIES).expect("parse memories");
    let queries = parse_queries(QUERIES).expect("parse queries");

    let memory_ids: HashSet<_> = memories.iter().map(|row| row.id.as_str()).collect();
    for query in queries.iter().filter(|query| !query.is_negative()) {
        assert!(
            query
                .expected
                .iter()
                .all(|graded| memory_ids.contains(graded.id.as_str())),
            "positive query {} references an unknown memory id",
            query.id
        );
    }

    let negatives: Vec<_> = queries.iter().filter(|query| query.is_negative()).collect();
    assert!(
        negatives.len() >= 10,
        "off-topic floor fixture needs a credible negative set"
    );
    assert!(
        negatives
            .iter()
            .any(|query| query.source == "aionforge-eval-synthetic")
    );
    assert!(
        negatives
            .iter()
            .any(|query| query.source == "aionforge-eval-adjacent")
    );

    let positive_classes: HashSet<_> = queries
        .iter()
        .filter(|query| !query.is_negative())
        .map(|query| classify(&query.query))
        .collect();
    assert!(positive_classes.contains(&QueryClass::SingleHopFactual));
    assert!(positive_classes.contains(&QueryClass::MultiHop));
    assert!(positive_classes.contains(&QueryClass::Temporal));

    let scrub_items = memories
        .iter()
        .map(|m| (m.id.as_str(), m.text.as_str()))
        .chain(queries.iter().map(|q| (q.id.as_str(), q.query.as_str())));
    let violations = scrub_violations(scrub_items);
    assert!(
        violations.is_empty(),
        "fixture scrub violations: {violations:?}"
    );
}

#[tokio::test]
#[ignore = "on-demand: makes real embedder network calls; run with --ignored once the key is sourced"]
async fn floor_sweep() {
    let Ok(key) = std::env::var(KEY_ENV) else {
        println!(
            "skipping floor_sweep: {KEY_ENV} unset. Run:\n  \
             set -a && source ~/.aionforge/aionforge-redeploy.env && set +a && \
             cargo test -p aionforge-eval --test floor_sweep -- --ignored --nocapture"
        );
        return;
    };

    let memories = parse_memories(MEMORIES).expect("parse memories");
    let queries = parse_queries(QUERIES).expect("parse queries");

    let scrub_items = memories
        .iter()
        .map(|m| (m.id.as_str(), m.text.as_str()))
        .chain(queries.iter().map(|q| (q.id.as_str(), q.query.as_str())));
    let violations = scrub_violations(scrub_items);
    assert!(
        violations.is_empty(),
        "fixture scrub violations: {violations:?}"
    );

    // Embed every memory and query text exactly once via the REAL embedder.
    let identity = EmbedderModel {
        family: MODEL.to_string(),
        version: String::new(),
        dimension: DIMENSION,
    };
    let embedder = HttpEmbedder::new(
        ENDPOINT,
        MODEL,
        identity.clone(),
        Some(SecretString::from(key)),
        Duration::from_millis(30_000),
    )
    .expect("build embedder");

    let mut texts: Vec<String> = memories.iter().map(|m| m.text.clone()).collect();
    texts.extend(queries.iter().map(|q| q.query.clone()));
    let approx_tokens = estimate_tokens(&texts);
    let price_per_mtok = price_per_mtok();
    let estimated_cost = approx_tokens as f64 / 1_000_000.0 * price_per_mtok;
    println!(
        "floor_sweep fixture: {} memories, {} queries ({} off-topic); approx input tokens: {}; estimated embedding cost: ${:.4} at ${:.4}/1M tokens",
        memories.len(),
        queries.len(),
        queries.iter().filter(|query| query.is_negative()).count(),
        approx_tokens,
        estimated_cost,
        price_per_mtok
    );
    let vectors = embedder.embed(&texts).await.expect("embed fixture");
    let cache: HashMap<String, Embedding> = texts.into_iter().zip(vectors).collect();
    println!(
        "embedded {} fixture texts once (cached for the sweep)",
        cache.len()
    );

    let (store, id_map) = seed_store(&memories, &cache);
    let retriever = HybridRetriever::new(
        store,
        CachingEmbedder {
            cache,
            model: identity,
        },
        RetrieverConfig::default(),
    );

    let mut rows = Vec::new();
    for &floor in &SWEEP {
        rows.push(
            score_queries(&retriever, &queries, &id_map, Some(floor))
                .await
                .as_floor_report(floor),
        );
    }
    let floor_off = score_queries(&retriever, &queries, &id_map, Some(0.0)).await;
    let shipped_profile = score_queries(&retriever, &queries, &id_map, None).await;

    let report = SweepReport::new(K, rows);

    println!(
        "\nfloor  reject  false_rej  recall@{K}  ndcg@{K}\n----------------------------------------------"
    );
    for row in &report.rows {
        println!(
            "{:<6.2} {:<7.3} {:<10.3} {:<10.3} {:.3}",
            row.floor, row.rejection_rate, row.false_rejection_rate, row.recall_at_k, row.ndcg_at_k
        );
    }
    match report.best_floor(MAX_FALSE_REJECTION) {
        Some(best) => println!(
            "\nbest floor (<= {MAX_FALSE_REJECTION} false-rejection): {:.2} \
             — rejects {:.0}% of off-topic queries at no recall cost",
            best.floor,
            best.rejection_rate * 100.0
        ),
        None => println!("\nno floor cleared every off-topic query without dropping a positive"),
    }
    println!(
        "\narm              floor source       reject  false_rej  recall@{K}  ndcg@{K}\n-------------------------------------------------------------------"
    );
    println!(
        "{:<16} {:<18} {:<7.3} {:<10.3} {:<10.3} {:.3}",
        "floor off",
        "forced 0.00",
        floor_off.rejection_rate,
        floor_off.false_rejection_rate,
        floor_off.recall_at_k,
        floor_off.ndcg_at_k
    );
    println!(
        "{:<16} {:<18} {:<7.3} {:<10.3} {:<10.3} {:.3}",
        "shipped profile",
        "router defaults",
        shipped_profile.rejection_rate,
        shipped_profile.false_rejection_rate,
        shipped_profile.recall_at_k,
        shipped_profile.ndcg_at_k
    );

    let out = std::env::temp_dir().join("aionforge-eval-floor-sweep.json");
    let mut file = std::fs::File::create(&out).expect("create report file");
    report.write_json(&mut file).expect("write report");
    println!("wrote sweep report to {}", out.display());

    assert!(!report.rows.is_empty(), "the sweep produced rows");
}
