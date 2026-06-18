//! Agent and session identity kinds (02 §4.8, §4.9).
//!
//! Both carry only the reduced [`Identity`] block (no stats): they are control /
//! identity nodes, not retrievable memories. An [`Agent`] is the authoring
//! principal that signs writes; a [`MemSession`] is the conversational scope an agent
//! works within. A memory's author is carried on the memory itself (`Episode.agent_id`
//! and the signed `ProvenanceRecord`); a session links via `IN_SESSION` (02 §5).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::blocks::Identity;
use crate::ids::Id;
use crate::time::Timestamp;

/// The Beta-distribution trust parameters and derived score for one category
/// (02 §6.5).
///
/// `alpha`/`beta` are the success/failure pseudo-counts of a Beta posterior; the
/// `score` is the derived `[0, 1]` point estimate. Carries floats, so it derives
/// `PartialEq` only (no `Eq`/`Hash`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrustCategory {
    /// Beta success pseudo-count.
    pub alpha: f64,
    /// Beta failure pseudo-count.
    pub beta: f64,
    /// Derived `[0, 1]` point estimate of trust for this category.
    pub score: f64,
}

/// The per-category trust map carried on an agent (`Agent.trust_scores`, 02 §6.5).
///
/// Maps a trust category name to its [`TrustCategory`] parameters. This is a
/// recomputable cache: the canonical state is the append-only log of reliability
/// events, and storage rewrites a category here only when folding that log changes
/// the value. A `BTreeMap` keeps the rendered JSON key order canonical. Carries
/// floats, so it derives `PartialEq` only.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TrustScores(pub BTreeMap<String, TrustCategory>);

/// The lifecycle status of an agent (02 §4.8; DB `DEFAULT 'active'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// The agent is live and may author writes.
    #[default]
    Active,
    /// The agent has been retired; its key no longer signs new writes.
    Retired,
}

/// An authoring principal: the agent that signs and owns memory writes (02 §4.8).
///
/// The substrate stores only the public key — private keys never enter the
/// substrate. Carries [`TrustScores`] (floats), so it derives `PartialEq` only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    /// Shared identity block (reduced: no stats).
    pub identity: Identity,
    /// base64-encoded public key. The substrate never stores private keys.
    pub public_key: String,
    /// Writer model family.
    pub model_family: String,
    /// Writer model version, if known.
    pub model_version: Option<String>,
    /// Per-category trust map (Beta parameters + derived score, 02 §6.5).
    pub trust_scores: TrustScores,
    /// Lifecycle status (`active` / `retired`).
    pub status: AgentStatus,
}

impl Agent {
    /// The selene-db node label for this kind.
    pub const LABEL: &str = "Agent";
}

/// A conversational scope an agent works within (02 §4.9).
///
/// Episodes and facts link to it via `IN_SESSION` for session-scoped retrieval and
/// session-diversity. Carries [`Timestamp`] and open JSON, so it derives
/// `PartialEq` only.
///
/// Named `MemSession`, not `Session`, because `SESSION` is a reserved keyword in
/// selene-db's GQL grammar (1.3+) — a `:Session` label fails to parse in DDL/`MATCH`.
/// The label string ([`MemSession::LABEL`]) is `"MemSession"` for the same reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemSession {
    /// Shared identity block (reduced: no stats).
    pub identity: Identity,
    /// When the session began.
    pub started_at: Timestamp,
    /// When the session ended; `None` while still open.
    pub ended_at: Option<Timestamp>,
    /// The agent that owns the session.
    pub owner_agent_id: Id,
    /// Open, caller-defined session metadata (intentionally unstructured, 02 §4.9).
    pub metadata: serde_json::Value,
}

impl MemSession {
    /// The selene-db node label for this kind. `"MemSession"` rather than `"Session"`:
    /// `SESSION` is a reserved GQL keyword in selene-db 1.3+, so `:Session` will not parse.
    pub const LABEL: &str = "MemSession";
}
