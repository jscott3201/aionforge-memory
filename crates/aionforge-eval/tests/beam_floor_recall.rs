//! On-demand BEAM source-recall-under-floor measurement against the REAL embedder.
//!
//! This answers the question PR #282 left open: how much *genuinely relevant* recall does
//! the dense `min_relevance` floor amputate on real data? It uses the BEAM long-term-memory
//! benchmark — real multi-domain conversations whose probing questions cite their evidence
//! messages (`source_chat_ids`). Each conversation's messages are seeded as episodes, each
//! probe is a query, and its evidence messages are the retrieval *gold*. We then measure, on
//! real gemini-3072 embeddings:
//!
//! 1. a **uniform floor sweep** — recall@k / rejection / false-rejection as one floor is
//!    applied to every query (the abstract "how much would a dense gate cut" curve);
//! 2. the **production config** (`min_relevance: None`) — the shipped per-class floors
//!    (SingleHopFactual at 0.60, others off), i.e. what actually ships;
//! 3. the **SingleHopFactual-only** slice — the floor only touches that class, so this is
//!    the honest blast-radius of the live 0.60 floor.
//!
//! It is `#[ignore]` and gated on BOTH the embedder key AND the external BEAM data path, so
//! it never runs in CI and never blocks on data that is not present.
//!
//! BEAM (`Mohammadta/BEAM`) is **CC BY-SA 4.0 and never vendored**. Prepare the data first:
//!
//! ```bash
//! python3 crates/aionforge-eval/tools/prepare_beam.py --conversations 6
//! source ~/.aionforge/aionforge-redeploy.env   # provides AIONFORGE_EMBEDDER_API_KEY
//! cargo test -p aionforge-eval --test beam_floor_recall -- --ignored --nocapture
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
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::time::Timestamp;
use aionforge_embed::HttpEmbedder;
use aionforge_eval::{
    BeamMessage, FloorReport, SweepReport, is_rejected, ndcg_at_k, parse_conversations, ranked_ids,
    recall_at_k,
};
use aionforge_retrieval::{
    HybridRetriever, QueryClass, RecallBundle, RecallOptions, RecallQuery, RetrieverConfig,
};
use aionforge_store::{Store, StoreConfig};
use secrecy::SecretString;

const ENDPOINT: &str = "https://openrouter.ai/api/v1";
const MODEL: &str = "google/gemini-embedding-2";
const DIMENSION: u32 = 3072;
const T0: &str = "2026-01-01T00:00:00Z[UTC]";
const KEY_ENV: &str = "AIONFORGE_EMBEDDER_API_KEY";
/// Override the normalized BEAM JSONL path; defaults to the prepare_beam.py output.
const DATA_ENV: &str = "AIONFORGE_BEAM_DATA";

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

/// Embed every text once, deduped, in batched network calls.
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
        let vectors = embedder.embed(chunk).await.expect("embed batch");
        for (text, vector) in chunk.iter().zip(vectors) {
            cache.insert(text.clone(), vector);
        }
    }
    cache
}

fn seed_store(
    messages: &[BeamMessage],
    cache: &HashMap<String, Embedding>,
) -> (Arc<Store>, HashMap<String, Id>) {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: DIMENSION,
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

// The `#[ignore]` is the load-bearing CI gate: the workspace test runs
// (`cargo nextest run --workspace`) do NOT pass `--ignored`, so this never executes there
// and never makes a network call in CI. Do NOT add `--ignored` to the workspace run. The
// runtime key/data gates below are a second line of defense for an explicit local invocation.
#[tokio::test]
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

    // Accumulators, summed over every probe across every conversation.
    let mut sweep: Vec<Arm> = vec![Arm::default(); SWEEP.len()];
    let mut production = Arm::default();
    let mut factual_baseline = Arm::default(); // floor 0.0, SingleHopFactual probes only
    let mut factual_production = Arm::default(); // production floor, SingleHopFactual only
    let mut class_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut false_rej_by_class: BTreeMap<String, usize> = BTreeMap::new();
    // ability -> [baseline 0.0, floor 0.60, production]
    let mut by_ability: BTreeMap<String, [Arm; 3]> = BTreeMap::new();
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
        let cache = embed_all(&embedder, &texts).await;
        let (store, id_map) = seed_store(&conversation.messages, &cache);
        let retriever = HybridRetriever::new(
            store,
            CachingEmbedder {
                cache,
                model: identity.clone(),
            },
            RetrieverConfig::default(),
        );

        for probe in &conversation.probes {
            let gold: HashSet<Id> = probe
                .source_ids
                .iter()
                .filter_map(|sid| id_map.get(sid).copied())
                .collect();
            let grades: HashMap<Id, u8> = gold.iter().map(|id| (*id, 1u8)).collect();
            let negative = probe.is_negative() || gold.is_empty();

            // Production config: the router applies the live per-class floors.
            let prod = recall(&retriever, &probe.question, None).await;
            let class = format!("{:?}", prod.explanation.class);
            *class_counts.entry(class.clone()).or_default() += 1;
            let is_factual = prod.explanation.class == QueryClass::SingleHopFactual;

            // Named bundles for the per-ability / factual-only views.
            let base = recall(&retriever, &probe.question, Some(0.0)).await;
            let floored = recall(&retriever, &probe.question, Some(FACTUAL_FLOOR)).await;
            let ability = by_ability.entry(probe.ability.clone()).or_default();

            if negative {
                production.observe_negative(is_rejected(&prod));
                ability[0].observe_negative(is_rejected(&base));
                ability[1].observe_negative(is_rejected(&floored));
                ability[2].observe_negative(is_rejected(&prod));
            } else {
                let prod_ranked = ranked_ids(&prod);
                production.observe_positive(&prod_ranked, &gold, &grades);
                let ranked_set: HashSet<Id> = prod_ranked.iter().copied().collect();
                if gold.iter().all(|id| !ranked_set.contains(id)) {
                    *false_rej_by_class.entry(class.clone()).or_default() += 1;
                }
                ability[0].observe_positive(&ranked_ids(&base), &gold, &grades);
                ability[1].observe_positive(&ranked_ids(&floored), &gold, &grades);
                ability[2].observe_positive(&prod_ranked, &gold, &grades);
                if is_factual {
                    factual_baseline.observe_positive(&ranked_ids(&base), &gold, &grades);
                    factual_production.observe_positive(&prod_ranked, &gold, &grades);
                }
            }

            // The uniform floor sweep.
            for (arm, &floor) in sweep.iter_mut().zip(SWEEP.iter()) {
                let bundle = recall(&retriever, &probe.question, Some(floor)).await;
                if negative {
                    arm.observe_negative(is_rejected(&bundle));
                } else {
                    arm.observe_positive(&ranked_ids(&bundle), &gold, &grades);
                }
            }
        }
    }

    // ---- Report ----
    let report = SweepReport::new(
        K,
        sweep
            .iter()
            .zip(SWEEP.iter())
            .map(|(arm, &floor)| arm.floor_report(floor))
            .collect(),
    );

    println!(
        "\nBEAM source-recall under the dense floor — {} conversations, {} messages, {} probes",
        conversations.len(),
        total_messages,
        total_probes
    );
    println!("(real gemini-{DIMENSION}; gold = a probe's cited evidence messages)\n");

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
        "\nPRODUCTION CONFIG (per-class floors as shipped; SingleHopFactual=0.60, others off):"
    );
    println!(
        "  recall@5={:.3} recall@10={:.3} recall@20={:.3} ndcg@{K}={:.3} false_rej={:.3} abstention_reject={:.3}",
        production.recall_at(5),
        production.recall_at(10),
        production.recall_at(20),
        production.ndcg_mean(),
        production.false_rejection_rate(),
        production.rejection_rate(),
    );

    println!(
        "\nSingleHopFactual-ONLY (the only class the floor touches) — the honest blast radius:"
    );
    println!(
        "  baseline (floor 0.0):     recall@5={:.3} recall@10={:.3} recall@20={:.3}  (n={})",
        factual_baseline.recall_at(5),
        factual_baseline.recall_at(10),
        factual_baseline.recall_at(20),
        factual_baseline.positives as usize,
    );
    println!(
        "  production (floor 0.60):  recall@5={:.3} recall@10={:.3} recall@20={:.3}  false_rej={:.3}",
        factual_production.recall_at(5),
        factual_production.recall_at(10),
        factual_production.recall_at(20),
        factual_production.false_rejection_rate(),
    );
    let recall_cost = factual_baseline.recall_at(K) - factual_production.recall_at(K);
    println!("  >>> recall@{K} cost of the live 0.60 factual floor: {recall_cost:.3}");

    println!("\nROUTED QUERY-CLASS DISTRIBUTION (how representative the factual floor even is):");
    for (class, count) in &class_counts {
        let frac = *count as f64 / total_probes.max(1) as f64;
        let fr = false_rej_by_class.get(class).copied().unwrap_or(0);
        println!(
            "  {class:<18} {count:>4} ({:>5.1}%)  false_rej_in_prod={fr}",
            frac * 100.0
        );
    }

    println!("\nPER-ABILITY recall@{K} (baseline 0.0 / floored 0.60 / production):");
    for (ability, arms) in &by_ability {
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

    let out = std::env::temp_dir().join("aionforge-eval-beam-floor-sweep.json");
    let mut file = std::fs::File::create(&out).expect("create report file");
    report.write_json(&mut file).expect("write report");
    println!("\nwrote uniform sweep report to {}", out.display());

    assert!(!report.rows.is_empty(), "the sweep produced rows");
    assert!(production.positives > 0.0, "BEAM produced positive probes");
}
