//! LongMemEval retrieval scorer tests and on-demand real-data runner.

// The ignored real runner prints skip/run status and a compact result table.
#![allow(clippy::print_stdout)]

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_embed::HttpEmbedder;
use aionforge_eval::{
    DEFAULT_LONGMEMEVAL_DATA_PATH, GoldGranularity, IngestMode, LONGMEMEVAL_DATA_ENV,
    LongMemEvalArm, LongMemEvalScoringOptions, parse_longmemeval, score_longmemeval,
    score_ranked_ids, seed_sessions, seed_sessions_with_embeddings,
};
use aionforge_retrieval::RetrieverConfig;
use secrecy::SecretString;

const ENDPOINT: &str = "https://openrouter.ai/api/v1";
const MODEL: &str = "google/gemini-embedding-2";
const NATIVE_DIM: u32 = 3072;
const KEY_ENV: &str = "AIONFORGE_EMBEDDER_API_KEY";
const DIM: usize = 8;
const EPSILON: f64 = 1.0e-12;
const EMBED_BATCH: usize = 64;

#[derive(Clone)]
struct FakeEmbedder {
    model: EmbedderModel,
    overrides: HashMap<String, Vec<f32>>,
}

impl FakeEmbedder {
    fn new(overrides: HashMap<String, Vec<f32>>) -> Self {
        Self {
            model: EmbedderModel {
                family: "fake-longmemeval".to_string(),
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
            .unwrap_or_else(|| hash_vec(text, DIM))
    }
}

#[derive(Debug)]
struct FakeEmbedError;

impl fmt::Display for FakeEmbedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("fake embedder never fails")
    }
}

impl Error for FakeEmbedError {}

impl Embedder for FakeEmbedder {
    type Error = FakeEmbedError;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let out = inputs
            .iter()
            .map(|text| Embedding::new(self.vector(text)).expect("valid fake embedding"))
            .collect();
        async move { Ok(out) }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

#[derive(Clone)]
struct CachingEmbedder {
    cache: HashMap<String, Embedding>,
    model: EmbedderModel,
}

#[derive(Debug)]
struct CacheMiss(String);

impl fmt::Display for CacheMiss {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no cached embedding for {:?}", self.0)
    }
}

impl Error for CacheMiss {}

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

#[tokio::test]
async fn synthetic_fixture_scores_planted_turn_gold() {
    let corpus = parse_longmemeval(
        r#"
        {
          "sessions": [
            {
              "session_id": "alpha-session",
              "date": "2026-01-01",
              "turns": [
                {"turn_id": "alpha-gold", "role": "user", "content": "alpha launch evidence"},
                {"turn_id": "alpha-distractor", "role": "assistant", "content": "ordinary distractor"}
              ]
            },
            {
              "session_id": "beta-session",
              "date": "2026-01-02",
              "turns": [
                {"turn_id": "beta-gold", "role": "user", "content": "beta migration evidence"}
              ]
            }
          ],
          "questions": [
            {
              "question_id": "q-alpha",
              "question": "which turn contains the alpha launch answer?",
              "evidence_turn_ids": ["alpha-gold"]
            },
            {
              "question_id": "q-beta",
              "question": "where is the beta migration answer?",
              "evidence_turn_ids": ["beta-gold"]
            }
          ]
        }
        "#,
    )
    .expect("parse synthetic LongMemEval fixture");
    assert_eq!(corpus.gold_granularity, GoldGranularity::Turn);

    let embedder = FakeEmbedder::new(HashMap::from([
        ("alpha launch evidence".to_string(), topic(0, DIM)),
        (
            "which turn contains the alpha launch answer?".to_string(),
            topic(0, DIM),
        ),
        ("beta migration evidence".to_string(), topic(1, DIM)),
        (
            "where is the beta migration answer?".to_string(),
            topic(1, DIM),
        ),
        ("ordinary distractor".to_string(), topic(4, DIM)),
    ]));
    let outcome = seed_sessions(
        &corpus.sessions,
        IngestMode::RawTurns,
        Arc::new(embedder.clone()),
        DIM as u32,
    )
    .await
    .expect("seed synthetic fixture");

    let report = score_longmemeval(
        &corpus,
        &outcome,
        &[LongMemEvalArm::new(
            "rrf default",
            RetrieverConfig::default(),
        )],
        embedder,
        LongMemEvalScoringOptions {
            k: 1,
            limit: 3,
            ..LongMemEvalScoringOptions::default()
        },
    )
    .await
    .expect("score synthetic fixture");

    let row = &report.rows[0];
    assert_eq!(report.questions, 2);
    assert_eq!(row.questions, 2);
    assert_close(row.recall_at_k, 1.0);
    assert_close(row.ndcg_at_k, 1.0);
    assert_close(row.mrr, 1.0);
}

#[test]
fn ranked_id_metrics_include_mrr_and_discounted_rank() {
    let distractor = Id::from_content_hash(b"longmemeval-distractor");
    let gold = Id::from_content_hash(b"longmemeval-gold");
    let ranked = vec![distractor, gold];
    let grades = HashMap::from([(gold, 1)]);

    let score = score_ranked_ids(&ranked, &grades, 2);

    assert_close(score.recall_at_k, 1.0);
    assert_close(score.reciprocal_rank, 0.5);
    assert_close(score.ndcg_at_k, 1.0 / 3.0_f64.log2());
}

#[test]
fn parser_uses_session_granularity_when_only_session_gold_exists() {
    let corpus = parse_longmemeval(
        r#"
        {
          "sessions": [
            {
              "session_id": "session-only",
              "turns": [{"turn_id": "s-only-1", "content": "session-level evidence"}]
            }
          ],
          "queries": [
            {"id": "q-session", "query": "which session matters?", "answer_session_ids": ["session-only"]}
          ]
        }
        "#,
    )
    .expect("parse session-granularity LongMemEval fixture");

    assert_eq!(corpus.gold_granularity, GoldGranularity::Session);
    assert_eq!(corpus.questions[0].granularity, GoldGranularity::Session);
    assert_eq!(corpus.questions[0].gold[0].id, "session-only");
}

#[tokio::test]
#[ignore = "on-demand: reads external LongMemEval_S data and calls the real embedder"]
async fn longmemeval_s_real_embedder() {
    let Ok(key) = std::env::var(KEY_ENV) else {
        println!("skipping longmemeval_s_real_embedder: {KEY_ENV} unset");
        return;
    };
    let data_path = default_data_path();
    let Ok(data) = std::fs::read_to_string(&data_path) else {
        println!(
            "skipping longmemeval_s_real_embedder: no LongMemEval data at {}",
            data_path.display()
        );
        return;
    };
    let corpus = parse_longmemeval(&data).expect("parse LongMemEval_S data");
    let texts = corpus_texts(&corpus);
    let embedder = HttpEmbedder::new(
        ENDPOINT,
        MODEL,
        model_for(NATIVE_DIM),
        Some(SecretString::from(key)),
        Duration::from_millis(30_000),
    )
    .expect("build real embedder");
    let cache = embed_all(&embedder, &texts).await;
    let cache_embedder = CachingEmbedder {
        cache: cache.clone(),
        model: model_for(NATIVE_DIM),
    };
    let outcome = seed_sessions_with_embeddings(
        &corpus.sessions,
        IngestMode::RawTurns,
        &cache,
        Some(Arc::new(cache_embedder.clone())),
        NATIVE_DIM,
    )
    .await
    .expect("seed LongMemEval_S through ingest adapter");
    let report = score_longmemeval(
        &corpus,
        &outcome,
        &[LongMemEvalArm::new(
            "rrf default",
            RetrieverConfig::default(),
        )],
        cache_embedder,
        LongMemEvalScoringOptions::default(),
    )
    .await
    .expect("score LongMemEval_S");

    report
        .write_markdown(&mut std::io::stdout())
        .expect("write markdown report");
}

fn default_data_path() -> PathBuf {
    if let Ok(path) = std::env::var(LONGMEMEVAL_DATA_ENV) {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(DEFAULT_LONGMEMEVAL_DATA_PATH.trim_start_matches("~/"))
}

fn model_for(dim: u32) -> EmbedderModel {
    EmbedderModel {
        family: MODEL.to_string(),
        version: String::new(),
        dimension: dim,
    }
}

fn corpus_texts(corpus: &aionforge_eval::LongMemEvalCorpus) -> Vec<String> {
    let mut texts = Vec::new();
    for session in &corpus.sessions {
        for turn in &session.turns {
            texts.push(turn.content.clone());
        }
    }
    for question in &corpus.questions {
        texts.push(question.question.clone());
    }
    texts
}

async fn embed_all(embedder: &HttpEmbedder, texts: &[String]) -> HashMap<String, Embedding> {
    let mut unique = Vec::new();
    let mut seen = HashSet::new();
    for text in texts {
        if seen.insert(text.as_str()) {
            unique.push(text.clone());
        }
    }

    let mut cache = HashMap::new();
    for chunk in unique.chunks(EMBED_BATCH) {
        let vectors = embedder
            .embed(chunk)
            .await
            .expect("embed LongMemEval_S batch");
        for (text, vector) in chunk.iter().zip(vectors) {
            cache.insert(text.clone(), vector);
        }
    }
    cache
}

fn topic(axis: usize, dim: usize) -> Vec<f32> {
    let mut vector = vec![0.0; dim];
    vector[axis % dim] = 1.0;
    vector
}

fn hash_vec(text: &str, dim: usize) -> Vec<f32> {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut x = h | 1;
    let mut vector = Vec::with_capacity(dim);
    for _ in 0..dim {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let unit = (x >> 11) as f64 / (1u64 << 53) as f64;
        vector.push((unit * 2.0 - 1.0) as f32);
    }
    let norm = vector
        .iter()
        .map(|component| component * component)
        .sum::<f32>()
        .sqrt()
        .max(f32::EPSILON);
    vector.iter().map(|component| component / norm).collect()
}

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < EPSILON,
        "expected {actual} to be close to {expected}"
    );
}
