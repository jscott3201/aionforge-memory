//! On-demand comparison of embedders and output sizes for off-topic rejection.
//!
//! For each candidate embedder/size this measures the **separation margin**: the gap
//! between the highest dense similarity an off-topic (negative) query reaches and the
//! lowest dense similarity an on-topic gold memory reaches. Any `min_relevance` floor
//! inside that gap cleanly separates off-topic from on-topic, so a *wider* margin means
//! a more robust floor — the decision metric for choosing an embedder and its size.
//!
//! Gemini's smaller sizes are derived from its 3072-d vectors by Matryoshka truncation
//! (first-N components, renormalized) — no extra network calls. The other models are
//! embedded at their native default dimension.
//!
//! `#[ignore]` + key-gated: run on demand with
//! `source ~/.aionforge/aionforge-redeploy.env && \`
//! `cargo test -p aionforge-eval --test embedder_compare -- --ignored --nocapture`.

// The comparison table IS the deliverable; a human reads it. (See floor_sweep.rs.)
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
    MemoryRow, QueryRow, max_dense_similarity, min_gold_dense_similarity, parse_memories,
    parse_queries, ranked_ids, recall_at_k, rejection_rate, scrub_violations,
};
use aionforge_retrieval::{
    HybridRetriever, RecallBundle, RecallOptions, RecallQuery, RetrieverConfig,
};
use aionforge_store::{Store, StoreConfig};
use secrecy::SecretString;

const MEMORIES: &str = include_str!("../fixtures/corpus_memories.jsonl");
const QUERIES: &str = include_str!("../fixtures/corpus_queries.jsonl");

const ENDPOINT: &str = "https://openrouter.ai/api/v1";
const T0: &str = "2026-01-01T00:00:00Z[UTC]";
const KEY_ENV: &str = "AIONFORGE_EMBEDDER_API_KEY";
const K: usize = 5;

/// A model embedded once at its native default dimension. The OpenAI-compatible request
/// carries no `dimensions` field, so the API returns the model's default size.
struct Source {
    key: &'static str,
    model: &'static str,
    native_dim: u32,
}

const SOURCES: &[Source] = &[
    Source {
        key: "gemini",
        model: "google/gemini-embedding-2",
        native_dim: 3072,
    },
    Source {
        key: "codestral",
        model: "mistralai/codestral-embed-2505",
        native_dim: 1536,
    },
    Source {
        key: "qwen4b",
        model: "qwen/qwen3-embedding-4b",
        native_dim: 2560,
    },
    Source {
        key: "qwen8b",
        model: "qwen/qwen3-embedding-8b",
        native_dim: 4096,
    },
];

/// A config to evaluate: a source's vectors resized (Matryoshka) to `target_dim`.
struct Config {
    label: &'static str,
    source: &'static str,
    target_dim: usize,
}

const CONFIGS: &[Config] = &[
    Config {
        label: "gemini-3072",
        source: "gemini",
        target_dim: 3072,
    },
    Config {
        label: "gemini-1536",
        source: "gemini",
        target_dim: 1536,
    },
    Config {
        label: "gemini-768",
        source: "gemini",
        target_dim: 768,
    },
    Config {
        label: "codestral-1536",
        source: "codestral",
        target_dim: 1536,
    },
    Config {
        label: "qwen3-4b-2560",
        source: "qwen4b",
        target_dim: 2560,
    },
    Config {
        label: "qwen3-8b-4096",
        source: "qwen8b",
        target_dim: 4096,
    },
];

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
            .map(|i| {
                self.cache
                    .get(i)
                    .cloned()
                    .ok_or_else(|| CacheMiss(i.clone()))
            })
            .collect();
        async move { result }
    }
    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

fn ts() -> Timestamp {
    T0.parse().expect("valid timestamp")
}

/// Matryoshka resize: keep the first `target` components and renormalize to unit length.
fn resize(embedding: &Embedding, target: usize) -> Embedding {
    let take = target.min(embedding.dimension());
    Embedding::new(embedding.as_slice()[..take].to_vec())
        .expect("non-empty truncation")
        .normalized()
}

/// Embed every text once with a source model at its native dimension.
async fn embed_source(
    source: &Source,
    key: &str,
    texts: &[String],
) -> Result<Vec<Embedding>, String> {
    let identity = EmbedderModel {
        family: source.model.to_string(),
        version: String::new(),
        dimension: source.native_dim,
    };
    let embedder = HttpEmbedder::new(
        ENDPOINT,
        source.model,
        identity,
        Some(SecretString::from(key.to_string())),
        Duration::from_millis(60_000),
    )
    .map_err(|e| format!("{e}"))?;
    embedder.embed(texts).await.map_err(|e| format!("{e}"))
}

fn seed_store(
    memories: &[MemoryRow],
    cache: &HashMap<String, Embedding>,
    dim: u32,
) -> (Arc<Store>, HashMap<String, Id>) {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: dim,
    })
    .expect("open store");
    store.migrate(&ts()).expect("migrate store");
    let mut id_map = HashMap::new();
    for row in memories {
        let id = Id::generate();
        id_map.insert(row.id.clone(), id);
        let episode = Episode {
            identity: Identity {
                id,
                ingested_at: ts(),
                namespace: Namespace::Global,
                expired_at: None,
            },
            stats: Stats {
                importance: row.importance,
                trust: row.trust,
                last_access: ts(),
                access_count_recent: 0,
                referenced_count: 0,
                surprise: 0.1,
                is_pinned: false,
            },
            content: row.text.clone(),
            role: Role::User,
            captured_at: ts(),
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

async fn recall(
    retriever: &HybridRetriever<CachingEmbedder>,
    query: &str,
    floor: f64,
) -> RecallBundle {
    retriever
        .recall(RecallQuery {
            text: query.to_string(),
            principal: Principal::agent(Id::generate()),
            limit: 25,
            options: RecallOptions {
                fanout: 50,
                min_relevance: Some(floor),
                ..RecallOptions::default()
            },
        })
        .await
        .expect("recall")
}

fn gold_for(query: &QueryRow, id_map: &HashMap<String, Id>) -> HashSet<Id> {
    query
        .expected
        .iter()
        .filter(|g| g.grade > 0)
        .filter_map(|g| id_map.get(&g.id).copied())
        .collect()
}

/// One embedder/size comparison row.
struct Row {
    label: String,
    dim: usize,
    max_offtopic: f64,
    min_ontopic: f64,
    margin: f64,
    rec_floor: f64,
    reject: f64,
    recall: f64,
}

async fn analyze(
    config: &Config,
    source_vecs: &HashMap<String, Embedding>,
    memories: &[MemoryRow],
    queries: &[QueryRow],
) -> Row {
    // Resize every source vector to the target dimension (renormalized).
    let cache: HashMap<String, Embedding> = source_vecs
        .iter()
        .map(|(text, emb)| (text.clone(), resize(emb, config.target_dim)))
        .collect();
    let (store, id_map) = seed_store(memories, &cache, config.target_dim as u32);
    let retriever = HybridRetriever::new(
        store,
        CachingEmbedder {
            cache,
            model: EmbedderModel {
                family: config.label.to_string(),
                version: String::new(),
                dimension: config.target_dim as u32,
            },
        },
        RetrieverConfig::default(),
    );

    // Pass 1, floor 0.0: the off-topic ceiling and the on-topic gold floor.
    let mut max_offtopic = 0.0_f64;
    let mut min_ontopic = 1.0_f64;
    for query in queries {
        let bundle = recall(&retriever, &query.query, 0.0).await;
        if query.is_negative() {
            if let Some(ceiling) = max_dense_similarity(&bundle) {
                max_offtopic = max_offtopic.max(ceiling);
            }
        } else {
            let gold = gold_for(query, &id_map);
            if let Some(floor) = min_gold_dense_similarity(&bundle, &gold) {
                min_ontopic = min_ontopic.min(floor);
            }
        }
    }
    let margin = min_ontopic - max_offtopic;
    // The recommended floor sits in the gap; if there is no clean gap, fall back to the
    // midpoint anyway so the reject/recall columns still characterize the overlap.
    let rec_floor = (max_offtopic + min_ontopic) / 2.0;

    // Pass 2, at the recommended floor: rejection over negatives, recall over positives.
    let mut negatives: Vec<RecallBundle> = Vec::new();
    let mut recall_sum = 0.0_f64;
    let mut positives = 0.0_f64;
    for query in queries {
        let bundle = recall(&retriever, &query.query, rec_floor).await;
        if query.is_negative() {
            negatives.push(bundle);
        } else {
            let gold = gold_for(query, &id_map);
            recall_sum += recall_at_k(&ranked_ids(&bundle), &gold, K);
            positives += 1.0;
        }
    }
    let neg_refs: Vec<&RecallBundle> = negatives.iter().collect();

    Row {
        label: config.label.to_string(),
        dim: config.target_dim,
        max_offtopic,
        min_ontopic,
        margin,
        rec_floor,
        reject: rejection_rate(&neg_refs),
        recall: recall_sum / positives.max(1.0),
    }
}

#[tokio::test]
#[ignore = "on-demand: embeds the fixture with several real OpenRouter models; run with --ignored"]
async fn embedder_compare() {
    let Ok(key) = std::env::var(KEY_ENV) else {
        println!("skipping embedder_compare: {KEY_ENV} unset (source the deploy env to run)");
        return;
    };

    let memories = parse_memories(MEMORIES).expect("parse memories");
    let queries = parse_queries(QUERIES).expect("parse queries");
    let violations = scrub_violations(
        memories
            .iter()
            .map(|m| (m.id.as_str(), m.text.as_str()))
            .chain(queries.iter().map(|q| (q.id.as_str(), q.query.as_str()))),
    );
    assert!(
        violations.is_empty(),
        "fixture scrub violations: {violations:?}"
    );

    let mut texts: Vec<String> = memories.iter().map(|m| m.text.clone()).collect();
    texts.extend(queries.iter().map(|q| q.query.clone()));

    // Embed each source model once (its native dimension).
    let mut source_vecs: HashMap<&str, HashMap<String, Embedding>> = HashMap::new();
    for source in SOURCES {
        match embed_source(source, &key, &texts).await {
            Ok(vectors) => {
                println!(
                    "embedded {} texts with {} ({}d)",
                    vectors.len(),
                    source.model,
                    source.native_dim
                );
                source_vecs.insert(source.key, texts.iter().cloned().zip(vectors).collect());
            }
            Err(e) => println!("SKIP {} — embed failed: {e}", source.model),
        }
    }

    let mut rows = Vec::new();
    for config in CONFIGS {
        let Some(vecs) = source_vecs.get(config.source) else {
            continue; // its source model was unavailable
        };
        rows.push(analyze(config, vecs, &memories, &queries).await);
    }
    rows.sort_by(|a, b| b.margin.total_cmp(&a.margin));

    println!(
        "\nembedder         dim    off_ceil  on_floor  margin   rec_floor  reject  recall@{K}\n\
         ------------------------------------------------------------------------------------"
    );
    for row in &rows {
        println!(
            "{:<16} {:<6} {:<9.3} {:<9.3} {:<8.3} {:<10.3} {:<7.3} {:.3}",
            row.label,
            row.dim,
            row.max_offtopic,
            row.min_ontopic,
            row.margin,
            row.rec_floor,
            row.reject,
            row.recall
        );
    }
    if let Some(best) = rows.first() {
        println!(
            "\nwidest separation: {} (margin {:.3}) — a floor near {:.2} rejects {:.0}% of \
             off-topic at recall@{K} {:.2}",
            best.label,
            best.margin,
            best.rec_floor,
            best.reject * 100.0,
            best.recall
        );
    }

    assert!(
        !rows.is_empty(),
        "at least one embedder produced a comparison row"
    );
}
