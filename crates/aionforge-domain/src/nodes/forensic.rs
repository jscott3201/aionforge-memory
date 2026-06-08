//! The forensic kinds: signed write proofs, the audit log, and the promotion
//! ledger (02 §4.10–§4.12).
//!
//! These are not retrievable memories, so they carry only the reduced
//! [`Identity`] block (no [`crate::blocks::Stats`]), per 02 §3. They form the
//! substrate's tamper-evident provenance and accountability trail.

use serde::{Deserialize, Serialize};

use crate::blocks::Identity;
use crate::ids::Id;
use crate::time::Timestamp;

/// A signed write proof attesting who wrote a memory and under what trust (02 §4.10).
///
/// One provenance record is emitted per memory write and linked back to the memory
/// via `HAS_PROVENANCE`. The `signature` is a base64 Ed25519 signature over the
/// canonical encoding of the write, making the authorship and write-time trust
/// non-repudiable. Trust here is a snapshot at write time, so `trust_at_write` is a
/// float and the struct derives `PartialEq` only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProvenanceRecord {
    /// Shared identity block.
    pub identity: Identity,
    /// The memory this record proves the provenance of.
    pub subject_id: Id,
    /// The agent that performed the write.
    pub writer_agent_id: Id,
    /// Base64 Ed25519 signature over the canonical encoding (immutable).
    pub signature: String,
    /// The episodes the written memory was derived from.
    pub source_episode_ids: Vec<Id>,
    /// Writer model family.
    pub model_family: String,
    /// Writer model version, if known.
    pub model_version: Option<String>,
    /// The writer/derivation trust captured at write time.
    pub trust_at_write: f64,
}

impl ProvenanceRecord {
    /// The selene-db node label for this kind.
    pub const LABEL: &str = "ProvenanceRecord";
}

/// The kind of audit event recorded (02 §4.11).
///
/// Enumerates every lifecycle, governance, and guard event the substrate audits.
/// Serialized in `snake_case` to match the spec's `kind` vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    /// An episode was captured.
    Capture,
    /// A memory was soft-forgotten (expired).
    Forget,
    /// A memory was hard-purged (erased).
    Purge,
    /// A memory was quarantined pending review.
    Quarantine,
    /// A soft-forgotten memory was restored.
    Unforget,
    /// A fact or core block was attested.
    Attest,
    /// A candidate was promoted across namespaces.
    Promote,
    /// A memory was demoted (lost support).
    Demote,
    /// A core block was edited.
    CoreEdit,
    /// A skill was saved.
    SkillSave,
    /// A skill was deprecated.
    SkillDeprecate,
    /// A skill version diff was recorded.
    SkillVersionDiff,
    /// Entities/facts were canonicalized.
    Canonicalize,
    /// An episode cluster was summarized into a note (or a summary was skipped to bound
    /// lost detail; the payload's outcome distinguishes the two).
    Summarize,
    /// An episode cluster was distilled into a note by the optional, off-by-default LLM
    /// distiller (M3.T08) — the LLM-backed counterpart to [`Summarize`](AuditKind::Summarize),
    /// kept distinct so distillation lineage and the consolidating model family stay queryable
    /// for the cross-family guard (07 §T3, M6.T01). The payload records the model identity,
    /// endpoint, seed, and outcome (written, rejected-lossy, or declined).
    Distill,
    /// Note links were evolved.
    LinkEvolve,
    /// A skill was induced from experience.
    InduceSkill,
    /// An agent reliability score was updated.
    ReliabilityUpdate,
    /// Importance scores were recomputed.
    ImportanceRecompute,
    /// A consolidation pass failed.
    ConsolidationFailed,
    /// The subliminal-learning guard raised a warning.
    SubliminalGuardWarning,
    /// A write was rejected by namespace authorization: the agent is not permitted to write the
    /// target namespace (06 §1). The payload records the agent, the requested namespace, and the
    /// deny reason — the audit of a cross-namespace write attempt (07 §T9).
    NamespaceDenied,
    /// A write was rejected for clock skew.
    ClockSkewRejected,
    /// A signature failed verification.
    InvalidSignature,
    /// A signing key was rotated.
    KeyRotation,
    /// An agent was retired.
    AgentRetired,
}

/// A single forensic audit record — the highest-cardinality kind (02 §4.11).
///
/// Every consequential substrate action emits an audit event signed by the
/// substrate keypair, linked to its subject via the `AUDIT` edge. The `payload`
/// is an intentionally open, kind-specific JSON shape (see [`AuditEvent::payload`]
/// and 02 §6.4). Because `payload` is a [`serde_json::Value`] and `occurred_at` is
/// a [`Timestamp`], the struct derives `PartialEq` only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Shared identity block.
    pub identity: Identity,
    /// The kind of event.
    pub kind: AuditKind,
    /// The memory or node the event is about.
    pub subject_id: Id,
    /// The actor (agent/substrate) that performed the action.
    pub actor_id: Id,
    /// Kind-specific structured detail; an intentionally open shape (02 §6.4).
    pub payload: serde_json::Value,
    /// Substrate-keypair signature over the canonical encoding (immutable).
    pub signature: String,
    /// Event time: when the action occurred (immutable).
    pub occurred_at: Timestamp,
}

impl AuditEvent {
    /// The selene-db node label for this kind.
    pub const LABEL: &str = "AuditEvent";
}

/// The resolution status of a promotion candidate (02 §4.12).
///
/// Serialized in `snake_case`. Defaults to [`PromotionStatus::Pending`], the state
/// a ledger entry is created in (the storage layer applies the DB `DEFAULT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromotionStatus {
    /// Awaiting the attestation quorum.
    #[default]
    Pending,
    /// Quorum reached; the candidate was promoted.
    Promoted,
    /// Quorum failed or the candidate was contradicted.
    Rejected,
}

/// A cross-namespace promotion ledger entry (02 §4.12).
///
/// Records a fact's progress toward quorum-gated promotion (team → global): the
/// posterior probability, the number of attestations collected, and the eventual
/// resolution. `posterior` is a float, so the struct derives `PartialEq` only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Promotion {
    /// Shared identity block.
    pub identity: Identity,
    /// The fact being considered for promotion.
    pub candidate_fact_id: Id,
    /// The posterior probability the candidate is correct.
    pub posterior: f64,
    /// The number of attestations collected so far.
    pub k: u64,
    /// The current resolution status.
    pub status: PromotionStatus,
    /// When the candidate was resolved; `None` while pending.
    pub resolved_at: Option<Timestamp>,
    /// The promoted fact produced on success; `None` until promoted.
    pub promoted_fact_id: Option<Id>,
}

impl Promotion {
    /// The selene-db node label for this kind.
    pub const LABEL: &str = "Promotion";
}
