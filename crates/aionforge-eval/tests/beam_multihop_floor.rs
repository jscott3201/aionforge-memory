//! On-demand MultiHop dense-floor calibration against the REAL embedder.
//!
//! The MultiHop/Entity hybrid admission (dense-OR-signal) is implemented but dormant: its
//! exemption protects graph/support-recovered gold, but turning the floor ON for these classes
//! needs a dense-floor VALUE for the *dense-recovered* branch (hits with no graph/support
//! contribution). This runner calibrates that value the way `beam_temporal_floor` did for
//! Temporal — the separation window between on-topic gold dense cosine and off-topic ceiling —
//! sliced to the MultiHop class on real BEAM.
//!
//! BEAM exercises MultiHop heavily (~40% of routed probes) but routes ~nothing to Entity (its
//! NL questions are not bare proper-noun lookups), so this calibrates MultiHop and reports the
//! Entity routing count to confirm Entity must inherit MultiHop's value rather than be measured
//! here. Graph/Support are idle on this episode-only store, so every hit is dense/lexical — i.e.
//! exactly the dense branch the floor value governs (the exemption handles the rest).
//!
//! `AIONFORGE_BEAM_DIMS` (default `3072,1536`) A/Bs the dimension via Matryoshka truncation of
//! one embed pass. `#[ignore]`, gated on the embedder key AND external BEAM data — never CI.
//!
//! ```bash
//! python3 crates/aionforge-eval/tools/prepare_beam.py --conversations 20
//! source ~/.aionforge/aionforge-redeploy.env
//! cargo test -p aionforge-eval --test beam_multihop_floor -- --ignored --nocapture
//! ```

#![allow(clippy::print_stdout)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
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
    BeamMessage, is_rejected, max_dense_similarity, min_gold_dense_similarity, parse_conversations,
    ranked_ids, recall_at_k,
};
use aionforge_retrieval::{
    HybridRetriever, QueryClass, RecallBundle, RecallOptions, RecallQuery, RetrieverConfig,
};
use aionforge_store::{Store, StoreConfig};
use secrecy::SecretString;

const ENDPOINT: &str = "https://openrouter.ai/api/v1";
const MODEL: &str = "google/gemini-embedding-2";
const NATIVE_DIM: u32 = 3072;
const T0: &str = "2026-01-01T00:00:00Z[UTC]";
const KEY_ENV: &str = "AIONFORGE_EMBEDDER_API_KEY";
const DATA_ENV: &str = "AIONFORGE_BEAM_DATA";
const DIMS_ENV: &str = "AIONFORGE_BEAM_DIMS";

const SWEEP: [f64; 10] = [0.0, 0.30, 0.35, 0.40, 0.45, 0.50, 0.55, 0.60, 0.65, 0.70];
const K: usize = 10;
const KS: [usize; 3] = [5, 10, 20];
const LIMIT: usize = 25;
const FANOUT: usize = 64;
const EMBED_BATCH: usize = 64;
const EMBED_RETRIES: u32 = 5;
const MAX_FALSE_REJECTION: f64 = 0.0;
/// The class this runner calibrates, as the router's `classify` spells it for the header check.
const TARGET_CLASS: QueryClass = QueryClass::MultiHop;

/// Authored MultiHop-shaped OFF-TOPIC queries: each carries an associative cue (why/how/
/// relationship/between/cause/compare/versus) so it routes MultiHop, and asks about
/// world/general knowledge unrelated to BEAM's personal-project conversations.
const MULTIHOP_OFFTOPIC: [&str; 16] = [
    "Why did the Roman Empire collapse?",
    "How does photosynthesis relate to the carbon cycle?",
    "What is the relationship between inflation and unemployment?",
    "Compare democracy versus authoritarianism.",
    "How does the moon cause ocean tides?",
    "Why are coral reefs important to marine ecosystems?",
    "What is the connection between diet and heart disease?",
    "How does the printing press influence literacy?",
    "Why does the sky appear blue?",
    "What is the relationship between supply and demand?",
    "How are volcanoes and earthquakes related?",
    "Why do economies experience recessions?",
    "How does deforestation impact biodiversity?",
    "What causes the greenhouse effect?",
    "Compare renewable and fossil energy sources.",
    "Why is the ocean salty?",
];

#[derive(Default, Clone)]
struct Arm {
    positives: f64,
    negatives: f64,
    negatives_rejected: f64,
    recall: [f64; 3],
    false_rejected: f64,
}

impl Arm {
    fn observe_positive(&mut self, ranked: &[Id], gold: &HashSet<Id>) {
        self.positives += 1.0;
        for (slot, &k) in self.recall.iter_mut().zip(KS.iter()) {
            *slot += recall_at_k(ranked, gold, k);
        }
        let returned: HashSet<Id> = ranked.iter().copied().collect();
        if !gold.is_empty() && gold.iter().all(|id| !returned.contains(id)) {
            self.false_rejected += 1.0;
        }
    }

    fn observe_negative(&mut self, rejected: bool) {
        self.negatives += 1.0;
        if rejected {
            self.negatives_rejected += 1.0;
        }
    }

    fn recall_at(&self, k: usize) -> f64 {
        let idx = KS.iter().position(|&x| x == k).expect("k in KS");
        if self.positives == 0.0 {
            0.0
        } else {
            self.recall[idx] / self.positives
        }
    }

    fn rejection_rate(&self) -> f64 {
        if self.negatives == 0.0 {
            1.0
        } else {
            self.negatives_rejected / self.negatives
        }
    }

    fn false_rejection_rate(&self) -> f64 {
        if self.positives == 0.0 {
            0.0
        } else {
            self.false_rejected / self.positives
        }
    }
}

struct DimAcc {
    positive_sweep: Vec<Arm>,
    negative_sweep: Vec<Arm>,
    on_topic_floors: Vec<f64>,
    off_topic_ceilings: Vec<f64>,
    authored_negatives: usize,
    beam_negatives: usize,
    entity_routed: usize,
    misrouted: usize,
}

impl DimAcc {
    fn new() -> Self {
        Self {
            positive_sweep: vec![Arm::default(); SWEEP.len()],
            negative_sweep: vec![Arm::default(); SWEEP.len()],
            on_topic_floors: Vec::new(),
            off_topic_ceilings: Vec::new(),
            authored_negatives: 0,
            beam_negatives: 0,
            entity_routed: 0,
            misrouted: 0,
        }
    }
}

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid timestamp")
}

fn default_data_path() -> PathBuf {
    if let Ok(path) = std::env::var(DATA_ENV) {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join(".aionforge")
        .join("beam-data")
        .join("normalized")
        .join("beam-100k.jsonl")
}

fn target_dims() -> Vec<u32> {
    let Ok(raw) = std::env::var(DIMS_ENV) else {
        return vec![NATIVE_DIM, 1536];
    };
    let mut dims: Vec<u32> = raw
        .split(',')
        .filter_map(|piece| piece.trim().parse::<u32>().ok())
        .filter(|&d| d > 0 && d <= NATIVE_DIM)
        .collect();
    dims.dedup();
    if dims.is_empty() {
        vec![NATIVE_DIM, 1536]
    } else {
        dims
    }
}

fn model_for(dim: u32) -> EmbedderModel {
    EmbedderModel {
        family: MODEL.to_string(),
        version: String::new(),
        dimension: dim,
    }
}

fn resize(embedding: &Embedding, target: u32) -> Embedding {
    let take = (target as usize).min(embedding.dimension());
    Embedding::new(embedding.as_slice()[..take].to_vec())
        .expect("non-empty truncation")
        .normalized()
}

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

async fn embed_batch_retrying(embedder: &HttpEmbedder, chunk: &[String]) -> Vec<Embedding> {
    let mut attempt = 0u32;
    loop {
        match embedder.embed(chunk).await {
            Ok(vectors) => return vectors,
            Err(error) => {
                attempt += 1;
                assert!(
                    attempt < EMBED_RETRIES,
                    "embed batch failed after {attempt} attempts: {error}"
                );
                println!("embed batch transient error (attempt {attempt}), retrying: {error}");
                tokio::time::sleep(Duration::from_millis(500u64 << attempt)).await;
            }
        }
    }
}

async fn embed_all(embedder: &HttpEmbedder, texts: &[String]) -> HashMap<String, Embedding> {
    let mut unique: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for text in texts {
        if seen.insert(text.as_str()) {
            unique.push(text.clone());
        }
    }
    let mut cache = HashMap::new();
    for chunk in unique.chunks(EMBED_BATCH) {
        let vectors = embed_batch_retrying(embedder, chunk).await;
        for (text, vector) in chunk.iter().zip(vectors) {
            cache.insert(text.clone(), vector);
        }
    }
    cache
}

fn seed_store(
    messages: &[BeamMessage],
    cache: &HashMap<String, Embedding>,
    dim: u32,
) -> (Arc<Store>, HashMap<String, Id>) {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: dim,
    })
    .expect("open store");
    store.migrate(&ts(T0)).expect("migrate store");

    let mut id_map = HashMap::new();
    for message in messages {
        let id = Id::generate();
        id_map.insert(message.id.clone(), id);
        let episode = Episode {
            identity: Identity {
                id,
                ingested_at: ts(T0),
                namespace: Namespace::Global,
                expired_at: None,
            },
            stats: Stats {
                importance: 0.5,
                trust: 0.5,
                last_access: ts(T0),
                access_count_recent: 0,
                referenced_count: 0,
                surprise: 0.1,
                is_pinned: false,
            },
            content: message.text.clone(),
            role: Role::User,
            captured_at: ts(T0),
            agent_id: Id::generate(),
            session_id: None,
            content_hash: ContentHash::of(message.text.as_bytes()),
            embedding: cache.get(&message.text).cloned(),
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
    min_relevance: Option<f64>,
) -> RecallBundle {
    retriever
        .recall(RecallQuery {
            text: query.to_string(),
            principal: Principal::agent(Id::generate()),
            limit: LIMIT,
            options: RecallOptions {
                fanout: FANOUT,
                min_relevance,
                ..RecallOptions::default()
            },
        })
        .await
        .expect("recall")
}

async fn measure_negative(
    retriever: &HybridRetriever<CachingEmbedder>,
    query: &str,
    acc: &mut DimAcc,
) {
    let base = recall(retriever, query, Some(0.0)).await;
    if let Some(ceiling) = max_dense_similarity(&base) {
        acc.off_topic_ceilings.push(ceiling);
    }
    for (arm, &floor) in acc.negative_sweep.iter_mut().zip(SWEEP.iter()) {
        arm.observe_negative(is_rejected(&recall(retriever, query, Some(floor)).await));
    }
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = (((sorted.len() - 1) as f64) * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        f64::NAN
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

#[tokio::test]
#[ignore = "on-demand: makes real embedder network calls and needs external BEAM data; run with --ignored"]
async fn beam_multihop_floor() {
    let Ok(key) = std::env::var(KEY_ENV) else {
        println!("skipping beam_multihop_floor: {KEY_ENV} unset (source the embedder env first)");
        return;
    };
    let data_path = default_data_path();
    let Ok(data) = std::fs::read_to_string(&data_path) else {
        println!(
            "skipping beam_multihop_floor: no BEAM data at {} (run tools/prepare_beam.py, or set {DATA_ENV})",
            data_path.display()
        );
        return;
    };

    let conversations = parse_conversations(&data).expect("parse BEAM conversations");
    assert!(!conversations.is_empty(), "BEAM data has conversations");
    let dims = target_dims();

    let embedder = HttpEmbedder::new(
        ENDPOINT,
        MODEL,
        model_for(NATIVE_DIM),
        Some(SecretString::from(key)),
        Duration::from_millis(30_000),
    )
    .expect("build embedder");

    let authored: Vec<String> = MULTIHOP_OFFTOPIC.iter().map(|s| (*s).to_string()).collect();
    let authored_native = embed_all(&embedder, &authored).await;

    let mut accs: BTreeMap<u32, DimAcc> = dims.iter().map(|&d| (d, DimAcc::new())).collect();

    for conversation in &conversations {
        let mut texts: Vec<String> = conversation
            .messages
            .iter()
            .map(|m| m.text.clone())
            .collect();
        texts.extend(conversation.probes.iter().map(|p| p.question.clone()));
        let native = embed_all(&embedder, &texts).await;

        for &dim in &dims {
            let mut cache: HashMap<String, Embedding> = native
                .iter()
                .map(|(text, vector)| (text.clone(), resize(vector, dim)))
                .collect();
            for (text, vector) in &authored_native {
                cache.insert(text.clone(), resize(vector, dim));
            }
            let (store, id_map) = seed_store(&conversation.messages, &cache, dim);
            let retriever = HybridRetriever::new(
                store,
                CachingEmbedder {
                    cache,
                    model: model_for(dim),
                },
                RetrieverConfig::default(),
            );
            let acc = accs.get_mut(&dim).expect("dim acc");

            for probe in &conversation.probes {
                let prod = recall(&retriever, &probe.question, None).await;
                if prod.explanation.class == QueryClass::Entity {
                    acc.entity_routed += 1;
                }
                if prod.explanation.class != TARGET_CLASS {
                    continue;
                }
                let gold: HashSet<Id> = probe
                    .source_ids
                    .iter()
                    .filter_map(|sid| id_map.get(sid).copied())
                    .collect();
                if probe.is_negative() || gold.is_empty() {
                    acc.beam_negatives += 1;
                    measure_negative(&retriever, &probe.question, acc).await;
                } else {
                    let base = recall(&retriever, &probe.question, Some(0.0)).await;
                    if let Some(floor) = min_gold_dense_similarity(&base, &gold) {
                        acc.on_topic_floors.push(floor);
                    }
                    for (arm, &floor) in acc.positive_sweep.iter_mut().zip(SWEEP.iter()) {
                        let bundle = recall(&retriever, &probe.question, Some(floor)).await;
                        arm.observe_positive(&ranked_ids(&bundle), &gold);
                    }
                }
            }

            for query in &authored {
                let prod = recall(&retriever, query, None).await;
                if prod.explanation.class != TARGET_CLASS {
                    acc.misrouted += 1;
                    continue;
                }
                acc.authored_negatives += 1;
                measure_negative(&retriever, query, acc).await;
            }
        }
    }

    for &dim in &dims {
        print_dim_report(dim, &accs[&dim], conversations.len());
    }

    let native_acc = accs.get(&NATIVE_DIM).or_else(|| accs.values().next());
    assert!(
        native_acc.is_some_and(|acc| acc.positive_sweep[0].positives > 0.0),
        "BEAM produced MultiHop-routed positive probes"
    );
}

fn print_dim_report(dim: u32, acc: &DimAcc, conversations: usize) {
    let mut floors = acc.on_topic_floors.clone();
    floors.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    let mut ceilings = acc.off_topic_ceilings.clone();
    ceilings.sort_by(|a, b| a.partial_cmp(b).expect("finite"));

    let n_pos = acc.positive_sweep[0].positives as usize;
    println!(
        "\n================ DIM {dim} — MultiHop dense-floor calibration ================\n\
         {conversations} conversations | MultiHop positives={n_pos} | off-topic negatives={} \
         (authored={} + beam={}) | authored misrouted={} | BEAM probes routed Entity={}\n\
         (real gemini truncated to {dim}; graph idle on this episode-only store, so these are the \
         dense-recovered branch the floor value governs)\n",
        acc.authored_negatives + acc.beam_negatives,
        acc.authored_negatives,
        acc.beam_negatives,
        acc.misrouted,
        acc.entity_routed,
    );

    println!("SEPARATION WINDOW (dense cosine):");
    println!(
        "  on-topic gold floor : min={:.3} p10={:.3} mean={:.3}  (a floor must sit BELOW this)",
        floors.first().copied().unwrap_or(f64::NAN),
        percentile(&floors, 0.10),
        mean(&floors),
    );
    println!(
        "  off-topic ceiling   : max={:.3} p90={:.3} mean={:.3}  (a floor must sit ABOVE this)",
        ceilings.last().copied().unwrap_or(f64::NAN),
        percentile(&ceilings, 0.90),
        mean(&ceilings),
    );
    let p90 = percentile(&ceilings, 0.90);
    let p10 = percentile(&floors, 0.10);
    if p90 < p10 {
        println!(
            "  >>> robust window on p90/p10: ({p90:.3}, {p10:.3}) — candidate dense floor = {:.3}",
            (p90 + p10) / 2.0
        );
    } else {
        println!(
            "  >>> p90/p10 OVERLAP ({p90:.3} >= {p10:.3}) — no clean dense floor; see the sweep"
        );
    }

    let baseline_false_rej = acc.positive_sweep[0].false_rejection_rate();
    println!("\nDENSE FLOOR SWEEP (marg_fr = false_rej above the floor-0.0 base miss):");
    println!(
        "floor  reject  false_rej  marg_fr  recall@{K}\n-------------------------------------------------"
    );
    let mut best: Option<(f64, f64)> = None;
    for (i, &floor) in SWEEP.iter().enumerate() {
        let reject = acc.negative_sweep[i].rejection_rate();
        let false_rej = acc.positive_sweep[i].false_rejection_rate();
        let marginal = (false_rej - baseline_false_rej).max(0.0);
        let recall = acc.positive_sweep[i].recall_at(K);
        println!("{floor:<6.2} {reject:<7.3} {false_rej:<10.3} {marginal:<8.3} {recall:.3}");
        if marginal <= MAX_FALSE_REJECTION && best.is_none_or(|(_, r)| reject > r) {
            best = Some((floor, reject));
        }
    }
    match best {
        Some((floor, reject)) if floor > 0.0 => println!(
            "\nbest MultiHop dense floor (<= {MAX_FALSE_REJECTION} MARGINAL false-rej): {floor:.2} \
             — rejects {:.0}% of off-topic MultiHop queries at zero marginal recall cost",
            reject * 100.0
        ),
        _ => println!(
            "\nno non-zero MultiHop dense floor cleared off-topic at zero marginal recall cost"
        ),
    }
    println!(
        "NOTE: graph/support-recovered gold is protected by the hybrid exemption regardless of \
         this floor; this value governs only the dense-recovered branch. Entity routed {} BEAM \
         probes — it inherits this value (uncalibratable on BEAM).",
        acc.entity_routed,
    );
}
