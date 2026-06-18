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
    LongMemEvalArm, LongMemEvalCorpus, LongMemEvalScoringOptions, parse_longmemeval,
    parse_longmemeval_cases, score_longmemeval, score_ranked_ids, score_seeded_longmemeval_cases,
    scrub_violations, seed_sessions, seed_sessions_with_embeddings,
};
use aionforge_retrieval::RetrieverConfig;
use secrecy::SecretString;

const ENDPOINT: &str = "https://openrouter.ai/api/v1";
const MODEL: &str = "google/gemini-embedding-2";
const NATIVE_DIM: u32 = 3072;
const KEY_ENV: &str = "AIONFORGE_EMBEDDER_API_KEY";
const LIMIT_ENV: &str = "AIONFORGE_LONGMEMEVAL_LIMIT";
const PRICE_PER_MTOK_ENV: &str = "AIONFORGE_LONGMEMEVAL_PRICE_PER_MTOK";
const DIM: usize = 8;
const EPSILON: f64 = 1.0e-12;
const DEFAULT_LIMIT: usize = 30;
const EMBED_BATCH: usize = 16;
const DEFAULT_PRICE_PER_MTOK: f64 = 0.15;

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

#[test]
fn parser_preserves_real_schema_as_per_question_cases() {
    let cases = parse_longmemeval_cases(
        r#"
        [
          {
            "question_id": "q-real",
            "question_type": "single-session-user",
            "question": "What did Avery choose?",
            "answer": "Avery chose rust.",
            "question_date": "2023/05/30 (Tue) 23:40",
            "haystack_session_ids": ["distractor-session", "answer-session"],
            "haystack_dates": ["2023/05/20 (Sat) 02:21", "2023/05/20 (Sat) 02:57"],
            "haystack_sessions": [
              [
                {"role": "user", "content": "A distractor about tea."}
              ],
              [
                {"role": "user", "content": "Avery chose rust for the memory engine.", "has_answer": true},
                {"role": "assistant", "content": "That choice was recorded."}
              ]
            ],
            "answer_session_ids": ["answer-session"]
          }
        ]
        "#,
    )
    .expect("parse cleaned LongMemEval-style fixture");

    assert_eq!(cases.len(), 1);
    let case = &cases[0];
    assert_eq!(case.questions.len(), 1);
    assert_eq!(case.sessions.len(), 2);
    assert_eq!(case.questions[0].granularity, GoldGranularity::Turn);
    assert_eq!(case.questions[0].gold.len(), 1);
    assert_eq!(case.questions[0].gold[0].id, "q-real::1::answer-session::0");
    assert!(case.session_id_map.contains_key("answer-session"));
}

#[test]
fn parser_mints_unique_turn_ids_when_real_haystack_repeats_session_ids() {
    let cases = parse_longmemeval_cases(
        r#"
        [
          {
            "question_id": "q-repeat",
            "question": "Which repeated session turn is relevant?",
            "question_date": "2023/05/30 (Tue) 23:40",
            "haystack_session_ids": ["repeat-session", "repeat-session"],
            "haystack_dates": ["2023/05/20 (Sat) 02:21", "2023/05/20 (Sat) 02:57"],
            "haystack_sessions": [
              [{"role": "user", "content": "first repeated session copy"}],
              [{"role": "user", "content": "second repeated session copy", "has_answer": true}]
            ],
            "answer_session_ids": ["repeat-session"]
          }
        ]
        "#,
    )
    .expect("parse repeated-session LongMemEval fixture");

    let fixture_ids: Vec<_> = cases[0]
        .sessions
        .iter()
        .flat_map(|session| session.turns.iter().map(|turn| turn.fixture_id.as_str()))
        .collect();
    assert_eq!(
        fixture_ids,
        vec![
            "q-repeat::0::repeat-session::0",
            "q-repeat::1::repeat-session::0"
        ]
    );
    assert_eq!(
        cases[0].questions[0].gold[0].id,
        "q-repeat::1::repeat-session::0"
    );
}

#[tokio::test]
async fn seeded_cases_score_each_question_haystack_independently() {
    let cases = parse_longmemeval_cases(
        r#"
        [
          {
            "question_id": "q-real-a",
            "question": "Where is alpha?",
            "question_date": "2023/05/30 (Tue) 23:40",
            "haystack_session_ids": ["alpha-session", "beta-session"],
            "haystack_dates": ["2023/05/20 (Sat) 02:21", "2023/05/20 (Sat) 02:57"],
            "haystack_sessions": [
              [{"role": "user", "content": "alpha answer text", "has_answer": true}],
              [{"role": "user", "content": "beta distractor text"}]
            ],
            "answer_session_ids": ["alpha-session"]
          },
          {
            "question_id": "q-real-b",
            "question": "Where is beta?",
            "question_date": "2023/05/30 (Tue) 23:40",
            "haystack_session_ids": ["alpha-session", "beta-session"],
            "haystack_dates": ["2023/05/20 (Sat) 02:21", "2023/05/20 (Sat) 02:57"],
            "haystack_sessions": [
              [{"role": "user", "content": "alpha distractor text"}],
              [{"role": "user", "content": "beta answer text", "has_answer": true}]
            ],
            "answer_session_ids": ["beta-session"]
          }
        ]
        "#,
    )
    .expect("parse cleaned LongMemEval-style cases");
    let embedder = FakeEmbedder::new(HashMap::from([
        ("Where is alpha?".to_string(), topic(0, DIM)),
        ("alpha answer text".to_string(), topic(0, DIM)),
        ("alpha distractor text".to_string(), topic(3, DIM)),
        ("Where is beta?".to_string(), topic(1, DIM)),
        ("beta answer text".to_string(), topic(1, DIM)),
        ("beta distractor text".to_string(), topic(4, DIM)),
    ]));

    let mut outcomes = Vec::new();
    for case in &cases {
        outcomes.push(
            seed_sessions(
                &case.sessions,
                IngestMode::RawTurns,
                Arc::new(embedder.clone()),
                DIM as u32,
            )
            .await
            .expect("seed per-question case"),
        );
    }
    let seeded: Vec<_> = cases.iter().zip(outcomes.iter()).collect();
    let report = score_seeded_longmemeval_cases(
        &seeded,
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
    .expect("score seeded cases");

    let row = &report.rows[0];
    assert_eq!(report.questions, 2);
    assert_close(row.recall_at_k, 1.0);
    assert_close(row.ndcg_at_k, 1.0);
    assert_close(row.mrr, 1.0);
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
    let mut cases = parse_longmemeval_cases(&data).expect("parse LongMemEval_S data");
    let total_questions = cases.len();
    let subset_limit = subset_limit();
    cases.truncate(subset_limit);
    assert_cases_scrubbed(&cases);
    assert_cases_have_unique_fixture_ids(&cases);
    let texts = cases_texts(&cases);
    let approx_tokens = estimate_tokens(&texts);
    let price_per_mtok = price_per_mtok();
    let estimated_cost = approx_tokens as f64 / 1_000_000.0 * price_per_mtok;
    println!(
        "LongMemEval_S subset: {} / {} questions; approx input tokens: {}; estimated embedding cost: ${:.4} at ${:.4}/1M tokens",
        cases.len(),
        total_questions,
        approx_tokens,
        estimated_cost,
        price_per_mtok
    );
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
    let mut outcomes = Vec::new();
    for case in &cases {
        outcomes.push(
            seed_sessions_with_embeddings(
                &case.sessions,
                IngestMode::RawTurns,
                &cache,
                Some(Arc::new(cache_embedder.clone())),
                NATIVE_DIM,
            )
            .await
            .expect("seed LongMemEval_S case through ingest adapter"),
        );
    }
    let seeded: Vec<_> = cases.iter().zip(outcomes.iter()).collect();
    let report = score_seeded_longmemeval_cases(
        &seeded,
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

fn subset_limit() -> usize {
    std::env::var(LIMIT_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(DEFAULT_LIMIT)
}

fn price_per_mtok() -> f64 {
    std::env::var(PRICE_PER_MTOK_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|price| *price >= 0.0)
        .unwrap_or(DEFAULT_PRICE_PER_MTOK)
}

fn cases_texts(cases: &[LongMemEvalCorpus]) -> Vec<String> {
    let mut texts = Vec::new();
    for case in cases {
        for session in &case.sessions {
            for turn in &session.turns {
                texts.push(turn.content.clone());
            }
        }
        for question in &case.questions {
            texts.push(question.question.clone());
        }
    }
    texts
}

fn assert_cases_scrubbed(cases: &[LongMemEvalCorpus]) {
    let items = cases.iter().flat_map(|case| {
        case.sessions.iter().flat_map(|session| {
            session
                .turns
                .iter()
                .map(|turn| (turn.fixture_id.as_str(), turn.content.as_str()))
        })
    });
    let violations = scrub_violations(items);
    assert!(
        violations.is_empty(),
        "LongMemEval_S subset still has scrub violations: {violations:?}"
    );
}

fn assert_cases_have_unique_fixture_ids(cases: &[LongMemEvalCorpus]) {
    for case in cases {
        let mut seen = HashSet::new();
        for turn in case
            .sessions
            .iter()
            .flat_map(|session| session.turns.iter())
        {
            assert!(
                seen.insert(turn.fixture_id.as_str()),
                "duplicate LongMemEval fixture id {} in question {}",
                turn.fixture_id,
                case.questions
                    .first()
                    .map(|question| question.id.as_str())
                    .unwrap_or("<unknown>")
            );
        }
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
        let vectors = embed_batch_retrying(embedder, chunk).await;
        for (text, vector) in chunk.iter().zip(vectors) {
            cache.insert(text.clone(), vector);
        }
    }
    cache
}

async fn embed_batch_retrying(embedder: &HttpEmbedder, chunk: &[String]) -> Vec<Embedding> {
    let mut delay = Duration::from_secs(2);
    for attempt in 1..=3 {
        match embedder.embed(chunk).await {
            Ok(vectors) => return vectors,
            Err(error) if attempt < 3 => {
                println!("LongMemEval_S embed batch attempt {attempt} failed: {error}; retrying");
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
            Err(error) => panic!("embed LongMemEval_S batch failed after retries: {error}"),
        }
    }
    unreachable!("retry loop returns or panics")
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
