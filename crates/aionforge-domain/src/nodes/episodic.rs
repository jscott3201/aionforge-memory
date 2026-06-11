//! The episodic memory tier: raw captured events (02 §4.1).

use serde::{Deserialize, Serialize};

use crate::blocks::{Identity, Stats};
use crate::embedding::{EmbedderModel, Embedding};
use crate::ids::{ContentHash, Id};
use crate::time::Timestamp;

/// The role of the actor that produced an episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// A human / user turn.
    User,
    /// A model / assistant turn.
    Assistant,
    /// A tool invocation or result.
    Tool,
    /// A system-role message (excluded from default recall, 07 §4).
    System,
    /// A non-conversational event.
    Event,
}

/// The consolidation lifecycle state of an episode — the consolidator's work key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsolidationState {
    /// Captured, not yet consolidated.
    #[default]
    Raw,
    /// Consolidation in progress.
    InProgress,
    /// Consolidated into derived facts/notes.
    Consolidated,
    /// Consolidation failed; see the `consolidation_failed` audit event.
    Failed,
}

/// A single redaction applied at capture (02 §6.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Redaction {
    /// The id of the pattern that matched.
    pub pattern_id: String,
    /// The `[start, end)` byte span in the original content.
    pub span: (usize, usize),
    /// The kind of sensitive data redacted.
    pub kind: String,
}

/// Structured origin and redaction metadata recorded on capture (`Episode.origin`, 02 §6.1).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Origin {
    /// Writer model family.
    pub model_family: Option<String>,
    /// Writer model version.
    pub model_version: Option<String>,
    /// Transport the capture arrived on.
    pub transport: Option<String>,
    /// Correlating request id.
    pub request_id: Option<String>,
    /// Redactions applied to the content.
    pub redactions: Vec<Redaction>,
    /// Ids of detected prompt-injection markers.
    pub injection_flags: Vec<String>,
    /// Capture latency in milliseconds.
    pub capture_latency_ms: Option<u64>,
    /// A writer-asserted supersession hint (04 §1 step 3): the id of a live episode this
    /// capture replaces, validated at capture against the writer's writable namespaces.
    /// Evidence for consolidation's supersession pass, never a capture-time action — the
    /// funnel records the claim; the pass decides what (if anything) it retires. Optional
    /// and absent-skipped so pre-hint episodes round-trip byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<Id>,
}

/// A raw captured event: the episodic tier, immutable and append-only (02 §4.1).
///
/// Everything derived — facts, entities, notes, summaries — references the episode
/// it came from via `DERIVED_FROM`/`MENTIONS`; the episode itself is never mutated
/// after commit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Episode {
    /// Shared identity block.
    pub identity: Identity,
    /// Shared stats block.
    pub stats: Stats,
    /// The raw event body.
    pub content: String,
    /// The role of the producing actor.
    pub role: Role,
    /// Event time: when it happened (immutable).
    pub captured_at: Timestamp,
    /// The authoring agent.
    pub agent_id: Id,
    /// The owning session, if any.
    pub session_id: Option<Id>,
    /// blake3 of the normalized content; the deduplication key.
    pub content_hash: ContentHash,
    /// Content embedding, if computed on the capture path.
    pub embedding: Option<Embedding>,
    /// Identity of the model that produced the embedding.
    pub embedder_model: Option<EmbedderModel>,
    /// The consolidation lifecycle state.
    pub consolidation_state: ConsolidationState,
    /// Structured origin / redaction metadata.
    pub origin: Option<Origin>,
}

impl Episode {
    /// The selene-db node label for this kind.
    pub const LABEL: &str = "Episode";
}
