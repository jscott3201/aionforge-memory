//! Hybrid retrieval: lexical/dense/graph/recency/trust signals, RRF fusion, query-class router, and the recall bundle.
//!
//! This implements the lexical and dense signals ([`lexical_ranking`] and
//! [`dense_ranking`], M1.T03–T04) and the associative graph signal (Personalized
//! PageRank seeded on query-mention entities, M3.T01) — each turns a query into a
//! best-first ranked candidate list — the deterministic Reciprocal Rank Fusion that
//! merges them ([`fuse`], M1.T05), the mandatory query-class router ([`route`], M1.T06)
//! that picks the mode weights and gates graph expansion, and the [`HybridRetriever`]
//! (M1.T07) that runs the whole path and returns a deterministic [`RecallBundle`] (03).
//! The recency and trust signals land with their tasks.

mod bundle;
mod error;
mod fusion;
mod precision;
mod query;
mod retriever;
mod router;
mod signals;
mod temporal;

pub use bundle::{
    EpisodeEntry, FactEntry, RecallBundle, RecallExplanation, StageTimings, StructuredEntry, render,
};
pub use error::RetrievalError;
pub use fusion::{Contribution, DEFAULT_RRF_K, FusedCandidate, WeightedRanking, fuse};
pub use query::{RecallOptions, RecallQuery, TemporalMode};
pub use retriever::{HybridRetriever, RetrieverConfig};
pub use router::{QueryClass, RetrievalProfile, SignalWeights, classify, profile_for, route};
pub use signals::{
    DenseRanking, RankedCandidate, Signal, SignalRanking, dense_ranking, lexical_ranking,
};
