//! Retrieval-quality evaluation harness for Aionforge Memory.
//!
//! This crate measures *off-topic rejection* and ranking quality of the retrieval
//! layer — the ground truth the RRF / signal-fusion work needs before any per-class
//! relevance floor is turned on or any fusion strategy is changed. It is a **leaf
//! crate**: it depends on the core libraries (`aionforge-retrieval`, `aionforge-domain`)
//! but nothing in the product depends on it, so it is never compiled into the shipped
//! `aionforge` binary. Heavy machinery (the real network embedder, the store seed path)
//! is confined to dev-dependencies and an on-demand runner, so the always-compiled
//! surface here stays small.
//!
//! The always-on modules are:
//! - [`metrics`] — pure functions over the public [`aionforge_retrieval::RecallBundle`]:
//!   `recall@k`, `nDCG@k`, off-topic-rejection rate, and a false-rejection guard.
//! - [`report`] — a serde-serializable sweep report over a range of floor values.
//! - [`fixture`] — the JSONL loader for the labeled corpus (memories + graded /
//!   negative queries).
//! - [`beam`] — a loader for a normalized slice of the external BEAM long-term-memory
//!   benchmark (never vendored), used by the on-demand source-recall-under-floor runner.
//! - [`ingest_adapter`] — a session/time-aware seed path for benchmark conversations.
//! - [`longmemeval`] — a LongMemEval retrieval-label loader and Recall@k/nDCG@k/MRR
//!   scorer over seeded benchmark sessions.
//! - [`scrub`] — a secret/PII gate run over any fixture before it becomes a baseline.
//!
//! The embedder-backed floor-sweep *runner* (which embeds a fixture once and re-runs
//! recall across a `min_relevance` sweep) is an on-demand, key-gated integration test —
//! never part of CI, never a shipped artifact.

pub mod beam;
pub mod fixture;
pub mod ingest_adapter;
pub mod longmemeval;
pub mod metrics;
pub mod report;
pub mod scrub;

pub use beam::{BeamConversation, BeamMessage, BeamProbe, parse_conversations};
pub use fixture::{Graded, MemoryRow, QueryRow, parse_memories, parse_queries};
pub use ingest_adapter::{
    IngestCorpus, IngestError, IngestMode, IngestOutcome, IngestSession, IngestTurn, seed_sessions,
    seed_sessions_with_embeddings,
};
pub use longmemeval::{
    DEFAULT_LONGMEMEVAL_DATA_PATH, GoldGranularity, LONGMEMEVAL_DATA_ENV, LongMemEvalArm,
    LongMemEvalArmReport, LongMemEvalCorpus, LongMemEvalError, LongMemEvalGold,
    LongMemEvalQuestion, LongMemEvalQuestionScore, LongMemEvalReport, LongMemEvalScoringOptions,
    parse_longmemeval, parse_longmemeval_cases, score_longmemeval, score_ranked_ids,
    score_seeded_longmemeval_cases,
};
pub use metrics::{
    CorpusMetrics, community_redundancy, false_rejection_rate, is_rejected, max_dense_similarity,
    min_gold_dense_similarity, ndcg_at_k, ranked_ids, recall_at_k, rejection_rate,
};
pub use report::{FloorReport, SweepReport};
pub use scrub::scrub_violations;
