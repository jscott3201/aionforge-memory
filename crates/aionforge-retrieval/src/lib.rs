//! Hybrid retrieval: lexical/dense/graph/recency/trust signals, RRF fusion, query-class router, and the recall bundle.
//!
//! This milestone implements the lexical and dense [`signals`] (M1.T03–T04) — each
//! turns a query into a best-first ranked candidate list — and [`fusion`] (M1.T05),
//! the deterministic Reciprocal Rank Fusion that merges them (03 §1–§2). The graph,
//! recency, and trust signals, the query-class router, and the recall bundle land
//! with their tasks.

mod error;
mod fusion;
mod signals;

pub use error::RetrievalError;
pub use fusion::{Contribution, DEFAULT_RRF_K, FusedCandidate, WeightedRanking, fuse};
pub use signals::{
    DenseRanking, RankedCandidate, Signal, SignalRanking, dense_ranking, lexical_ranking,
};
