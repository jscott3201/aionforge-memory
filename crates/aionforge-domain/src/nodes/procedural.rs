//! The procedural memory tier: reusable procedures and their negative
//! counterparts (02 §4.4, §4.5).
//!
//! [`Skill`] is a versioned, reliability-scored procedure stored as data;
//! [`BadPattern`] is the negative procedural memory recording a failure mode so
//! it can be avoided. Both are retrievable memory kinds and compose the
//! [`Identity`] and [`Stats`] blocks (02 §3).

use serde::{Deserialize, Serialize};

use crate::blocks::{Identity, Stats};
use crate::embedding::{EmbedderModel, Embedding};
use crate::ids::ContentHash;
use crate::time::Timestamp;

/// A versioned, reliability-scored procedure: the procedural tier (02 §4.4).
///
/// A skill is identified by `name` and disambiguated by a monotonic `version`;
/// the substrate deprecates (`deprecated_at`) rather than deletes, so the full
/// version history is retained. The `body` is the procedure stored as data and
/// is never executed by the substrate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skill {
    /// Shared identity block.
    pub identity: Identity,
    /// Shared stats block.
    pub stats: Stats,
    /// Skill name; stable across versions.
    pub name: String,
    /// Monotonic version, per `name`.
    pub version: i64,
    /// Human-readable description; a BM25 recall surface.
    pub description: String,
    /// "What question does this answer" embedding, if computed.
    pub problem_embedding: Option<Embedding>,
    /// Identity of the model that produced `problem_embedding`.
    pub embedder_model: Option<EmbedderModel>,
    /// Language tag of the skill body.
    pub language: String,
    /// The procedure itself, stored as data; size-bounded by config.
    pub body: String,
    /// Parameter schema (02 §4.4).
    ///
    /// Intentionally open-shaped (arbitrary JSON Schema-like document), so it is
    /// carried as a raw [`serde_json::Value`].
    pub params: serde_json::Value,
    /// Declared preconditions, if any (02 §4.4).
    ///
    /// Intentionally open-shaped; carried as a raw [`serde_json::Value`].
    pub preconditions: Option<serde_json::Value>,
    /// Declared postconditions / expected effects, if any (02 §4.4).
    ///
    /// Intentionally open-shaped; carried as a raw [`serde_json::Value`].
    pub postconditions: Option<serde_json::Value>,
    /// Declared capabilities (immutable per version).
    pub capabilities: Vec<String>,
    /// Count of recorded successful invocations.
    pub success_count: u64,
    /// Count of recorded failed invocations.
    pub failure_count: u64,
    /// Mean invocation latency in milliseconds, if measured.
    pub mean_latency_ms: Option<f64>,
    /// blake3 of `body`; the change-detection key.
    pub source_hash: ContentHash,
    /// Last successful-invocation instant.
    pub last_success_at: Option<Timestamp>,
    /// Last failed-invocation instant.
    pub last_failure_at: Option<Timestamp>,
    /// Deprecation instant; `None` while active. Deprecate, never delete.
    pub deprecated_at: Option<Timestamp>,
    /// Whether the skill was auto-derived by consolidation (`DEFAULT FALSE`).
    pub induced: bool,
}

impl Skill {
    /// The selene-db node label for this kind.
    pub const LABEL: &str = "Skill";
}

/// A skill retrieved by problem similarity, paired with the reliability-weighted score that
/// ranked it (05; M3.T04).
///
/// Procedural retrieval ranks active skills by how well their stored problem matches the query
/// *and* how reliable they have proven in practice, so the score that ordered the list is kept
/// alongside the skill — and split into its two factors — so a caller can see *why* a skill
/// surfaced, not just that it did. Carries an f64 score, so it derives `PartialEq` only.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedSkill {
    /// The retrieved skill (always a live, active, non-deprecated version).
    pub skill: Skill,
    /// The problem-match score: rank fusion over the vector (problem-embedding) and lexical
    /// (description) signals. Higher is better.
    pub similarity: f64,
    /// The reliability weight: the Beta-posterior mean of the skill's success rate, so an
    /// unproven skill sits at the neutral prior rather than at either extreme.
    pub reliability: f64,
    /// The final rank score, `similarity * reliability`; the returned list is ordered by this,
    /// descending.
    pub score: f64,
}

/// A negative procedural memory: a recorded failure mode to avoid (02 §4.5).
///
/// Linked to the skill it was observed against via `HAS_FAILURE`; drives
/// Reflexion-style avoidance during planning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BadPattern {
    /// Shared identity block.
    pub identity: Identity,
    /// Shared stats block.
    pub stats: Stats,
    /// The failure mode, described in natural language.
    pub description: String,
    /// Content embedding of the failure mode, if computed.
    pub embedding: Option<Embedding>,
    /// Identity of the model that produced `embedding`.
    pub embedder_model: Option<EmbedderModel>,
    /// Event time: when the failure was observed (immutable).
    pub observed_at: Timestamp,
}

impl BadPattern {
    /// The selene-db node label for this kind.
    pub const LABEL: &str = "BadPattern";
}
