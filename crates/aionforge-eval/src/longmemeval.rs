//! LongMemEval retrieval-only loader and scorer.
//!
//! This module evaluates whether retrieval surfaces the evidence turns or
//! sessions that LongMemEval labels as answer-bearing. It deliberately stops
//! before answer generation or judge scoring: callers seed the haystack with the
//! ingest adapter, run recall, and score the retrieval trace with recall@k,
//! nDCG@k, and MRR.

use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
    io::{self, Write},
    sync::Arc,
};

use aionforge_domain::{Retriever, authz::Principal, contracts::Embedder, ids::Id};
use aionforge_retrieval::{
    HybridRetriever, RecallOptions, RecallQuery, RetrievalError, RetrieverConfig,
};
use serde::{Deserialize, Serialize};

use crate::{IngestOutcome, ndcg_at_k, ranked_ids, recall_at_k};

mod parser;

pub use parser::parse_longmemeval;

/// Environment variable for an external LongMemEval data file.
pub const LONGMEMEVAL_DATA_ENV: &str = "AIONFORGE_LONGMEMEVAL_DATA";

/// Default local path, mirroring the BEAM external-data convention.
pub const DEFAULT_LONGMEMEVAL_DATA_PATH: &str = "~/.aionforge/longmemeval-data/LongMemEval_S.json";

/// The label space used for gold evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoldGranularity {
    /// Gold labels identify answer-bearing turns/messages.
    Turn,
    /// Gold labels identify answer-bearing sessions.
    Session,
    /// The loaded corpus contained both turn- and session-level questions.
    Mixed,
}

/// One labeled LongMemEval retrieval query.
#[derive(Debug, Clone, PartialEq)]
pub struct LongMemEvalQuestion {
    /// Stable question id.
    pub id: String,
    /// Natural-language retrieval query.
    pub question: String,
    /// Gold evidence labels in the question's [`Self::granularity`] space.
    pub gold: Vec<LongMemEvalGold>,
    /// Whether labels are turn ids or session ids.
    pub granularity: GoldGranularity,
}

/// One gold evidence label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LongMemEvalGold {
    /// Fixture turn id or source session id.
    pub id: String,
    /// Relevance grade; `1` for ungraded evidence.
    pub grade: u8,
}

/// Normalized LongMemEval corpus ready for adapter seeding and retrieval scoring.
#[derive(Debug, Clone, PartialEq)]
pub struct LongMemEvalCorpus {
    /// Haystack sessions normalized for the ingest adapter.
    pub sessions: Vec<crate::IngestSession>,
    /// Labeled retrieval questions.
    pub questions: Vec<LongMemEvalQuestion>,
    /// Original session labels to the deterministic ids used by [`crate::IngestSession`].
    pub session_id_map: HashMap<String, Id>,
    /// Finest granularity available across the corpus.
    pub gold_granularity: GoldGranularity,
}

impl LongMemEvalCorpus {
    /// Build a corpus from already-normalized pieces.
    #[must_use]
    pub fn new(
        sessions: Vec<crate::IngestSession>,
        questions: Vec<LongMemEvalQuestion>,
        session_id_map: HashMap<String, Id>,
    ) -> Self {
        let gold_granularity = corpus_granularity(&questions);
        Self {
            sessions,
            questions,
            session_id_map,
            gold_granularity,
        }
    }
}

/// One retrieval configuration arm in an A/B run.
#[derive(Debug, Clone)]
pub struct LongMemEvalArm {
    /// Display name used in reports.
    pub name: String,
    /// Retriever-level configuration for this arm.
    pub config: RetrieverConfig,
}

impl LongMemEvalArm {
    /// Create a named scoring arm.
    #[must_use]
    pub fn new(name: impl Into<String>, config: RetrieverConfig) -> Self {
        Self {
            name: name.into(),
            config,
        }
    }
}

/// Scoring knobs shared by every arm.
#[derive(Debug, Clone, PartialEq)]
pub struct LongMemEvalScoringOptions {
    /// Cutoff for recall@k and nDCG@k.
    pub k: usize,
    /// Recall bundle size.
    pub limit: usize,
    /// Per-query options. Defaults to the production path.
    pub recall_options: RecallOptions,
}

impl Default for LongMemEvalScoringOptions {
    fn default() -> Self {
        Self {
            k: 10,
            limit: 25,
            recall_options: RecallOptions::default(),
        }
    }
}

/// Corpus-level report for all requested arms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LongMemEvalReport {
    /// Cutoff used for recall@k and nDCG@k.
    pub k: usize,
    /// Number of retrieval questions scored.
    pub questions: usize,
    /// Gold label space used by the corpus.
    pub gold_granularity: GoldGranularity,
    /// One row per A/B arm.
    pub rows: Vec<LongMemEvalArmReport>,
}

impl LongMemEvalReport {
    /// Write a compact markdown table.
    ///
    /// # Errors
    /// Propagates writer I/O errors.
    pub fn write_markdown<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        writeln!(
            writer,
            "| arm | questions | recall@{} | nDCG@{} | MRR |",
            self.k, self.k
        )?;
        writeln!(writer, "|---|---:|---:|---:|---:|")?;
        for row in &self.rows {
            writeln!(
                writer,
                "| {} | {} | {:.3} | {:.3} | {:.3} |",
                row.arm, row.questions, row.recall_at_k, row.ndcg_at_k, row.mrr
            )?;
        }
        Ok(())
    }
}

/// One report row for a scorer arm.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LongMemEvalArmReport {
    /// Arm display name.
    pub arm: String,
    /// Number of questions scored.
    pub questions: usize,
    /// Mean recall@k.
    pub recall_at_k: f64,
    /// Mean nDCG@k.
    pub ndcg_at_k: f64,
    /// Mean reciprocal rank.
    pub mrr: f64,
}

/// Per-question metric values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LongMemEvalQuestionScore {
    /// Recall@k for one question.
    pub recall_at_k: f64,
    /// nDCG@k for one question.
    pub ndcg_at_k: f64,
    /// Reciprocal rank for the first relevant hit, or `0.0` on a miss.
    pub reciprocal_rank: f64,
}

/// Loader/scorer errors.
#[derive(Debug)]
pub enum LongMemEvalError {
    /// Input JSON could not be parsed or normalized.
    Parse(String),
    /// Gold labels could not be mapped into the seeded store id space.
    MissingGold(String),
    /// Retrieval failed.
    Retrieval(RetrievalError),
}

impl fmt::Display for LongMemEvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(message) => write!(f, "LongMemEval parse error: {message}"),
            Self::MissingGold(message) => write!(f, "LongMemEval gold mapping error: {message}"),
            Self::Retrieval(error) => write!(f, "LongMemEval retrieval error: {error}"),
        }
    }
}

impl Error for LongMemEvalError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Retrieval(error) => Some(error),
            Self::Parse(_) | Self::MissingGold(_) => None,
        }
    }
}

impl From<RetrievalError> for LongMemEvalError {
    fn from(error: RetrievalError) -> Self {
        Self::Retrieval(error)
    }
}

/// Score one set of seeded sessions under a list of retriever arms.
///
/// # Errors
/// Returns [`LongMemEvalError`] if retrieval fails or gold labels cannot be mapped.
pub async fn score_longmemeval<E>(
    corpus: &LongMemEvalCorpus,
    outcome: &IngestOutcome,
    arms: &[LongMemEvalArm],
    embedder: E,
    options: LongMemEvalScoringOptions,
) -> Result<LongMemEvalReport, LongMemEvalError>
where
    E: Embedder + Clone + Send + Sync + 'static,
{
    let index = ScoringIndex::new(corpus, outcome);
    let mut rows = Vec::new();
    for arm in arms {
        let retriever =
            HybridRetriever::new(Arc::clone(&outcome.store), embedder.clone(), arm.config);
        let mut aggregate = ArmAggregate::default();
        for question in &corpus.questions {
            let bundle = retriever
                .recall(RecallQuery {
                    text: question.question.clone(),
                    principal: Principal::agent(Id::from_content_hash(
                        b"aionforge-eval-longmemeval-scorer",
                    )),
                    limit: options.limit,
                    options: options.recall_options.clone(),
                })
                .await?;
            let ranked = index.ranked_for(question.granularity, &ranked_ids(&bundle));
            let grades = index.grades_for(question)?;
            aggregate.observe(score_ranked_ids(&ranked, &grades, options.k));
        }
        rows.push(aggregate.finish(arm.name.clone()));
    }

    Ok(LongMemEvalReport {
        k: options.k,
        questions: corpus.questions.len(),
        gold_granularity: corpus.gold_granularity,
        rows,
    })
}

/// Score one ranked list against graded gold labels.
#[must_use]
pub fn score_ranked_ids(
    ranked: &[Id],
    grades: &HashMap<Id, u8>,
    k: usize,
) -> LongMemEvalQuestionScore {
    let gold: HashSet<Id> = grades
        .iter()
        .filter_map(|(id, grade)| (*grade > 0).then_some(*id))
        .collect();
    let rank = ranked.iter().position(|id| gold.contains(id));
    LongMemEvalQuestionScore {
        recall_at_k: recall_at_k(ranked, &gold, k),
        ndcg_at_k: ndcg_at_k(ranked, grades, k),
        reciprocal_rank: rank.map_or(0.0, |r| 1.0 / (r as f64 + 1.0)),
    }
}

#[derive(Default)]
struct ArmAggregate {
    questions: usize,
    recall_sum: f64,
    ndcg_sum: f64,
    rr_sum: f64,
}

impl ArmAggregate {
    fn observe(&mut self, score: LongMemEvalQuestionScore) {
        self.questions += 1;
        self.recall_sum += score.recall_at_k;
        self.ndcg_sum += score.ndcg_at_k;
        self.rr_sum += score.reciprocal_rank;
    }

    fn finish(self, arm: String) -> LongMemEvalArmReport {
        let denom = self.questions.max(1) as f64;
        LongMemEvalArmReport {
            arm,
            questions: self.questions,
            recall_at_k: self.recall_sum / denom,
            ndcg_at_k: self.ndcg_sum / denom,
            mrr: self.rr_sum / denom,
        }
    }
}

struct ScoringIndex {
    episode_to_session: HashMap<Id, Id>,
    turn_to_episode: HashMap<String, Id>,
    session_id_map: HashMap<String, Id>,
}

impl ScoringIndex {
    fn new(corpus: &LongMemEvalCorpus, outcome: &IngestOutcome) -> Self {
        let mut episode_to_session = HashMap::new();
        for session in &corpus.sessions {
            for turn in &session.turns {
                if let Some(episode_id) = outcome.id_map.get(&turn.fixture_id) {
                    episode_to_session.insert(*episode_id, session.session_id);
                }
            }
        }
        Self {
            episode_to_session,
            turn_to_episode: outcome.id_map.clone(),
            session_id_map: corpus.session_id_map.clone(),
        }
    }

    fn ranked_for(&self, granularity: GoldGranularity, ranked: &[Id]) -> Vec<Id> {
        match granularity {
            GoldGranularity::Turn | GoldGranularity::Mixed => ranked.to_vec(),
            GoldGranularity::Session => {
                let mut seen = HashSet::new();
                ranked
                    .iter()
                    .filter_map(|id| self.episode_to_session.get(id).copied())
                    .filter(|id| seen.insert(*id))
                    .collect()
            }
        }
    }

    fn grades_for(
        &self,
        question: &LongMemEvalQuestion,
    ) -> Result<HashMap<Id, u8>, LongMemEvalError> {
        let mut grades = HashMap::new();
        for gold in &question.gold {
            let id = match question.granularity {
                GoldGranularity::Turn | GoldGranularity::Mixed => self
                    .turn_to_episode
                    .get(&gold.id)
                    .copied()
                    .ok_or_else(|| LongMemEvalError::MissingGold(gold.id.clone()))?,
                GoldGranularity::Session => self
                    .session_id_map
                    .get(&gold.id)
                    .copied()
                    .unwrap_or_else(|| session_id(&gold.id)),
            };
            grades.insert(id, gold.grade);
        }
        Ok(grades)
    }
}

fn corpus_granularity(questions: &[LongMemEvalQuestion]) -> GoldGranularity {
    let has_turn = questions
        .iter()
        .any(|q| matches!(q.granularity, GoldGranularity::Turn));
    let has_session = questions
        .iter()
        .any(|q| matches!(q.granularity, GoldGranularity::Session));
    match (has_turn, has_session) {
        (true, true) => GoldGranularity::Mixed,
        (false, true) => GoldGranularity::Session,
        (true, false) | (false, false) => GoldGranularity::Turn,
    }
}

fn session_id(label: &str) -> Id {
    Id::from_content_hash(format!("longmemeval-session|{label}").as_bytes())
}
