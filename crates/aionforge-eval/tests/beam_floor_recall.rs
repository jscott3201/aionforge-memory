//! On-demand BEAM source-recall-under-floor measurement against the REAL embedder.
//!
//! This answers the question PR #282 left open: how much *genuinely relevant* recall does
//! the dense `min_relevance` floor amputate on real data? It uses the BEAM long-term-memory
//! benchmark — real multi-domain conversations whose probing questions cite their evidence
//! messages (`source_chat_ids`). Each conversation's messages are seeded as episodes, each
//! probe is a query, and its evidence messages are the retrieval *gold*. We then measure, on
//! real gemini embeddings:
//!
//! 1. a **uniform floor sweep** — recall@k / rejection / false-rejection as one floor is
//!    applied to every query (the abstract "how much would a dense gate cut" curve);
//! 2. the **production config** (`min_relevance: None`) — the shipped per-class floors
//!    (SingleHopFactual at 0.60, others off), i.e. what actually ships;
//! 3. the **SingleHopFactual-only** slice — the floor only touches that class, so this is
//!    the honest blast-radius of the live 0.60 floor.
//!
//! It also sweeps **embedding dimension** (`AIONFORGE_BEAM_DIMS`, default `3072`): the model
//! is embedded once at its native 3072 and each smaller dimension is a Matryoshka truncation
//! (first-N components, renormalized) of the *same* vectors — no extra network calls — so a
//! gemini-1536-vs-3072 A/B costs one embedding pass. This evaluates the half-dimension
//! Matryoshka option on real data before any production re-embed.
//!
//! It is `#[ignore]` and gated on BOTH the embedder key AND the external BEAM data path, so
//! it never runs in CI and never blocks on data that is not present.
//!
//! BEAM (`Mohammadta/BEAM`) is **CC BY-SA 4.0 and never vendored**. Prepare the data first:
//!
//! ```bash
//! python3 crates/aionforge-eval/tools/prepare_beam.py --conversations 6
//! source ~/.aionforge/aionforge-redeploy.env   # provides AIONFORGE_EMBEDDER_API_KEY
//! AIONFORGE_BEAM_DIMS=3072,1536 \
//!   cargo test -p aionforge-eval --test beam_floor_recall -- --ignored --nocapture
//! ```

// This runner's output IS its deliverable: a human reads the printed tables. The workspace
// keeps print_stdout at warn for exactly such cases; allow it here.
#![allow(clippy::print_stdout)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aionforge_domain::Retriever;
use aionforge_domain::authz::Principal;
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_embed::HttpEmbedder;
use aionforge_eval::{
    BeamConversation, FloorReport, IngestMode, IngestSession, SweepReport, is_rejected, ndcg_at_k,
    parse_conversations, ranked_ids, recall_at_k, seed_sessions_with_embeddings,
};
use aionforge_retrieval::{
    HybridRetriever, QueryClass, RecallBundle, RecallOptions, RecallQuery, RetrieverConfig,
};
use aionforge_store::Store;
use secrecy::SecretString;

const ENDPOINT: &str = "https://openrouter.ai/api/v1";
const MODEL: &str = "google/gemini-embedding-2";
/// The model's native embedding dimension (what the API returns without a `dimensions` field).
const NATIVE_DIM: u32 = 3072;
const KEY_ENV: &str = "AIONFORGE_EMBEDDER_API_KEY";
/// Override the normalized BEAM JSONL path; defaults to the prepare_beam.py output.
const DATA_ENV: &str = "AIONFORGE_BEAM_DATA";
/// Comma-separated target embedding dimensions to A/B (Matryoshka truncations of the native
/// vector). Default `3072` (native only). Each must be <= NATIVE_DIM.
const DIMS_ENV: &str = "AIONFORGE_BEAM_DIMS";

/// Uniform floors to sweep. `0.0` is OFF (baseline); `0.60` is the live factual floor.
const SWEEP: [f64; 10] = [0.0, 0.35, 0.40, 0.45, 0.50, 0.55, 0.60, 0.62, 0.65, 0.70];
/// The live SingleHopFactual floor (router `FACTUAL_FLOOR`), reported as a named arm.
const FACTUAL_FLOOR: f64 = 0.60;
/// The cutoff for the headline recall@k / nDCG@k (BEAM probes often cite multiple,
/// far-apart evidence messages, so a slightly wider k than the off-topic sweep).
const K: usize = 10;
/// Recall is also reported at these k to show k-sensitivity.
const KS: [usize; 3] = [5, 10, 20];
/// Recall budget: large enough to admit recall@20 over a ~200-message conversation.
const LIMIT: usize = 25;
const FANOUT: usize = 64;
/// Embedding request batch size (one network call per chunk).
const EMBED_BATCH: usize = 64;

/// Serves pre-computed vectors from a cache so the multi-floor sweep re-runs recall with
/// zero network calls. Every text it is asked for is warmed before the sweep.
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

/// Matryoshka resize: keep the first `target` components and renormalize to unit length.
fn resize(embedding: &Embedding, target: u32) -> Embedding {
    let take = (target as usize).min(embedding.dimension());
    Embedding::new(embedding.as_slice()[..take].to_vec())
        .expect("non-empty truncation")
        .normalized()
}

/// A running tally for one measurement arm (one floor, or the production config), summed
/// over every probe across every conversation.
#[derive(Default, Clone)]
struct Arm {
    positives: f64,
    negatives: f64,
    negatives_rejected: f64,
    false_rejected: f64,
    recall: [f64; 3],
    ndcg: f64,
}

impl Arm {
    fn observe_positive(&mut self, ranked: &[Id], gold: &HashSet<Id>, grades: &HashMap<Id, u8>) {
        self.positives += 1.0;
        for (slot, &k) in self.recall.iter_mut().zip(KS.iter()) {
            *slot += recall_at_k(ranked, gold, k);
        }
        self.ndcg += ndcg_at_k(ranked, grades, K);
        let ranked_set: HashSet<Id> = ranked.iter().copied().collect();
        if !gold.is_empty() && gold.iter().all(|id| !ranked_set.contains(id)) {
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

    fn ndcg_mean(&self) -> f64 {
        if self.positives == 0.0 {
            0.0
        } else {
            self.ndcg / self.positives
        }
    }

    fn false_rejection_rate(&self) -> f64 {
        if self.positives == 0.0 {
            0.0
        } else {
            self.false_rejected / self.positives
        }
    }

    fn rejection_rate(&self) -> f64 {
        if self.negatives == 0.0 {
            1.0
        } else {
            self.negatives_rejected / self.negatives
        }
    }

    fn floor_report(&self, floor: f64) -> FloorReport {
        FloorReport {
            floor,
            rejection_rate: self.rejection_rate(),
            false_rejection_rate: self.false_rejection_rate(),
            recall_at_k: self.recall_at(K),
            ndcg_at_k: self.ndcg_mean(),
        }
    }
}

/// All accumulators for one embedding dimension, summed over every probe and conversation.
struct DimAcc {
    sweep: Vec<Arm>,
    production: Arm,
    factual_baseline: Arm,
    factual_production: Arm,
    class_counts: BTreeMap<String, usize>,
    false_rej_by_class: BTreeMap<String, usize>,
    by_ability: BTreeMap<String, [Arm; 3]>,
}

impl DimAcc {
    fn new() -> Self {
        Self {
            sweep: vec![Arm::default(); SWEEP.len()],
            production: Arm::default(),
            factual_baseline: Arm::default(),
            factual_production: Arm::default(),
            class_counts: BTreeMap::new(),
            false_rej_by_class: BTreeMap::new(),
            by_ability: BTreeMap::new(),
        }
    }
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
        return vec![NATIVE_DIM];
    };
    let mut dims: Vec<u32> = raw
        .split(',')
        .filter_map(|piece| piece.trim().parse::<u32>().ok())
        .filter(|&d| d > 0 && d <= NATIVE_DIM)
        .collect();
    dims.dedup();
    if dims.is_empty() {
        vec![NATIVE_DIM]
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

/// How many times to retry one embedding batch on a transient failure. A long run makes
/// many requests, so a single network blip must not abort the whole measurement.
const EMBED_RETRIES: u32 = 5;

/// Embed one batch, retrying with exponential backoff on a transient failure. The error's
/// `Display` carries the endpoint URL but no header, so it never surfaces the key.
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
                let backoff = Duration::from_millis(500u64 << attempt);
                println!("embed batch transient error (attempt {attempt}), retrying: {error}");
                tokio::time::sleep(backoff).await;
            }
        }
    }
}

/// Embed every text once, deduped, in batched network calls (at the native dimension).
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

async fn seed_store(
    conversation: &BeamConversation,
    cache: &HashMap<String, Embedding>,
    dim: u32,
) -> (Arc<Store>, HashMap<String, Id>) {
    let session = IngestSession::from(conversation);
    let outcome = seed_sessions_with_embeddings::<CachingEmbedder>(
        &[session],
        IngestMode::RawTurns,
        cache,
        None,
        dim,
    )
    .await
    .expect("seed BEAM conversation through ingest adapter");
    (outcome.store, outcome.id_map)
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

/// Measure one probe against one dimension's retriever, folding the result into `acc`.
async fn measure_probe(
    retriever: &HybridRetriever<CachingEmbedder>,
    question: &str,
    ability: &str,
    gold: &HashSet<Id>,
    grades: &HashMap<Id, u8>,
    negative: bool,
    acc: &mut DimAcc,
) {
    let prod = recall(retriever, question, None).await;
    let class = format!("{:?}", prod.explanation.class);
    *acc.class_counts.entry(class.clone()).or_default() += 1;
    let is_factual = prod.explanation.class == QueryClass::SingleHopFactual;

    let base = recall(retriever, question, Some(0.0)).await;
    let floored = recall(retriever, question, Some(FACTUAL_FLOOR)).await;
    let arms = acc.by_ability.entry(ability.to_string()).or_default();

    if negative {
        acc.production.observe_negative(is_rejected(&prod));
        arms[0].observe_negative(is_rejected(&base));
        arms[1].observe_negative(is_rejected(&floored));
        arms[2].observe_negative(is_rejected(&prod));
    } else {
        let prod_ranked = ranked_ids(&prod);
        acc.production.observe_positive(&prod_ranked, gold, grades);
        let ranked_set: HashSet<Id> = prod_ranked.iter().copied().collect();
        if gold.iter().all(|id| !ranked_set.contains(id)) {
            *acc.false_rej_by_class.entry(class).or_default() += 1;
        }
        arms[0].observe_positive(&ranked_ids(&base), gold, grades);
        arms[1].observe_positive(&ranked_ids(&floored), gold, grades);
        arms[2].observe_positive(&prod_ranked, gold, grades);
        if is_factual {
            acc.factual_baseline
                .observe_positive(&ranked_ids(&base), gold, grades);
            acc.factual_production
                .observe_positive(&prod_ranked, gold, grades);
        }
    }

    for (arm, &floor) in acc.sweep.iter_mut().zip(SWEEP.iter()) {
        let bundle = recall(retriever, question, Some(floor)).await;
        if negative {
            arm.observe_negative(is_rejected(&bundle));
        } else {
            arm.observe_positive(&ranked_ids(&bundle), gold, grades);
        }
    }
}

fn print_dim_report(dim: u32, acc: &DimAcc, conversations: usize, messages: usize, probes: usize) {
    let report = SweepReport::new(
        K,
        acc.sweep
            .iter()
            .zip(SWEEP.iter())
            .map(|(arm, &floor)| arm.floor_report(floor))
            .collect(),
    );
    println!(
        "\n================ DIM {dim} ================\n\
         BEAM source-recall under the dense floor — {conversations} conversations, \
         {messages} messages, {probes} probes\n(real gemini truncated to {dim}; gold = a probe's \
         cited evidence messages)\n"
    );
    println!("UNIFORM FLOOR SWEEP (one floor applied to every query):");
    println!(
        "floor  reject  false_rej  recall@{K}  ndcg@{K}\n----------------------------------------------"
    );
    for row in &report.rows {
        println!(
            "{:<6.2} {:<7.3} {:<10.3} {:<10.3} {:.3}",
            row.floor, row.rejection_rate, row.false_rejection_rate, row.recall_at_k, row.ndcg_at_k
        );
    }
    println!(
        "\nPRODUCTION CONFIG (per-class floors; SingleHopFactual=0.60, others off):\n  \
         recall@5={:.3} recall@10={:.3} recall@20={:.3} false_rej={:.3} abstention_reject={:.3}",
        acc.production.recall_at(5),
        acc.production.recall_at(10),
        acc.production.recall_at(20),
        acc.production.false_rejection_rate(),
        acc.production.rejection_rate(),
    );
    println!(
        "\nSingleHopFactual-ONLY (the only class the floor touches):\n  \
         baseline (0.0):    recall@5={:.3} recall@10={:.3} recall@20={:.3}  (n={})\n  \
         production (0.60): recall@5={:.3} recall@10={:.3} recall@20={:.3}  false_rej={:.3}",
        acc.factual_baseline.recall_at(5),
        acc.factual_baseline.recall_at(10),
        acc.factual_baseline.recall_at(20),
        acc.factual_baseline.positives as usize,
        acc.factual_production.recall_at(5),
        acc.factual_production.recall_at(10),
        acc.factual_production.recall_at(20),
        acc.factual_production.false_rejection_rate(),
    );
    let cost = acc.factual_baseline.recall_at(K) - acc.factual_production.recall_at(K);
    println!("  >>> recall@{K} cost of the live 0.60 factual floor: {cost:.3}");

    println!("\nROUTED QUERY-CLASS DISTRIBUTION:");
    for (class, count) in &acc.class_counts {
        let frac = *count as f64 / probes.max(1) as f64;
        let fr = acc.false_rej_by_class.get(class).copied().unwrap_or(0);
        println!(
            "  {class:<18} {count:>4} ({:>5.1}%)  false_rej_in_prod={fr}",
            frac * 100.0
        );
    }

    println!("\nPER-ABILITY recall@{K} (baseline 0.0 / floored 0.60 / production):");
    for (ability, arms) in &acc.by_ability {
        println!(
            "  {ability:<24} {:.3} / {:.3} / {:.3}   (pos={})",
            arms[0].recall_at(K),
            arms[1].recall_at(K),
            arms[2].recall_at(K),
            arms[0].positives as usize,
        );
    }
    match report.best_floor(0.0) {
        Some(best) => println!(
            "\nbest uniform floor (<=0 false-rejection): {:.2} — rejects {:.0}% of abstention probes",
            best.floor,
            best.rejection_rate * 100.0
        ),
        None => println!("\nno uniform floor cleared every abstention probe at zero recall cost"),
    }
}

#[tokio::test]
// The `#[ignore]` is the load-bearing CI gate: the workspace test runs
// (`cargo nextest run --workspace`) do NOT pass `--ignored`, so this never executes there
// and never makes a network call in CI. Do NOT add `--ignored` to the workspace run. The
// runtime key/data gates below are a second line of defense for an explicit local invocation.
#[ignore = "on-demand: makes real embedder network calls and needs external BEAM data; run with --ignored"]
async fn beam_floor_recall() {
    let Ok(key) = std::env::var(KEY_ENV) else {
        println!("skipping beam_floor_recall: {KEY_ENV} unset (source the embedder env first)");
        return;
    };
    let data_path = default_data_path();
    let Ok(data) = std::fs::read_to_string(&data_path) else {
        println!(
            "skipping beam_floor_recall: no BEAM data at {} \
             (run tools/prepare_beam.py, or set {DATA_ENV})",
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

    let mut accs: BTreeMap<u32, DimAcc> = dims.iter().map(|&d| (d, DimAcc::new())).collect();
    let mut total_messages = 0usize;
    let mut total_probes = 0usize;

    for conversation in &conversations {
        total_messages += conversation.messages.len();
        total_probes += conversation.probes.len();

        let mut texts: Vec<String> = conversation
            .messages
            .iter()
            .map(|m| m.text.clone())
            .collect();
        texts.extend(conversation.probes.iter().map(|p| p.question.clone()));
        // Embed once at the native dimension; every target dim is a truncation of these.
        let native = embed_all(&embedder, &texts).await;

        for &dim in &dims {
            let cache: HashMap<String, Embedding> = native
                .iter()
                .map(|(text, vector)| (text.clone(), resize(vector, dim)))
                .collect();
            let (store, id_map) = seed_store(conversation, &cache, dim).await;
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
                let gold: HashSet<Id> = probe
                    .source_ids
                    .iter()
                    .filter_map(|sid| id_map.get(sid).copied())
                    .collect();
                let grades: HashMap<Id, u8> = gold.iter().map(|id| (*id, 1u8)).collect();
                let negative = probe.is_negative() || gold.is_empty();
                measure_probe(
                    &retriever,
                    &probe.question,
                    &probe.ability,
                    &gold,
                    &grades,
                    negative,
                    acc,
                )
                .await;
            }
        }
    }

    for &dim in &dims {
        print_dim_report(
            dim,
            &accs[&dim],
            conversations.len(),
            total_messages,
            total_probes,
        );
    }

    if dims.len() > 1 {
        println!("\n================ DIMENSION A/B (SingleHopFactual recall@{K}) ================");
        println!("dim    base(0.0)  prod(0.60)  floor_cost  prod_false_rej");
        for &dim in &dims {
            let acc = &accs[&dim];
            let cost = acc.factual_baseline.recall_at(K) - acc.factual_production.recall_at(K);
            println!(
                "{:<6} {:<10.3} {:<11.3} {:<11.3} {:.3}",
                dim,
                acc.factual_baseline.recall_at(K),
                acc.factual_production.recall_at(K),
                cost,
                acc.factual_production.false_rejection_rate(),
            );
        }
    }

    let native_acc = accs.get(&NATIVE_DIM).or_else(|| accs.values().next());
    assert!(
        native_acc.is_some_and(|acc| acc.production.positives > 0.0),
        "BEAM produced positive probes"
    );
}
