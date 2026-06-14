//! The edge kinds connecting memory nodes (02 §5).
//!
//! Every edge is a directed relationship between two node kinds. Bi-temporal edges
//! carry the four-timestamp [`BiTemporal`] validity block as a `temporal` field;
//! event time (`valid_from`/`valid_to`) records when the underlying relationship was
//! true in the world, transaction time (`ingested_at`/`expired_at`) records when the
//! substrate believed it. Edges with no extra properties and no bi-temporal block
//! are unit-like marker structs: their presence is the whole signal.
//!
//! Endpoint labels are documented per edge but not encoded in the type — the storage
//! layer enforces endpoint constraints against the node labels in [`crate::nodes`].
//! Per spec §5, `DERIVED_FROM` and `AUDIT` are polymorphic (endpoints relaxed); all
//! others enumerate their endpoint labels in the doc comment.

use serde::{Deserialize, Serialize};

use crate::time::{BiTemporal, Timestamp};

/// The label of every edge kind in the graph (02 §5).
///
/// Variants carry the exact selene-db relationship name (SCREAMING_SNAKE_CASE).
/// This is a closed enumeration of the relationship vocabulary; each edge struct
/// also exposes a matching `LABEL` constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeLabel {
    /// `Episode` → `Entity`: extraction provenance / entity-anchored retrieval.
    Mentions,
    /// `Fact` → `Entity`: canonical subject; carries the fact's validity window.
    About,
    /// `Fact`/`Episode` → `Fact`: graph-expanded support scoring.
    Supports,
    /// `Fact` → `Fact`: maintained current-state (unsuperseded) provider.
    SupersededBy,
    /// `Fact` → `Fact`: unresolved-current / quarantine provider.
    Contradicts,
    /// `Fact` → `ValidityAnchor`: current-valid coverage; point-in-time anchoring.
    ValidAt,
    /// memory → `Scope`: scope-membership provider (strongest precision filter).
    InScope,
    /// `Episode`/`Fact` → `Session`: session-scoped retrieval; session-diversity.
    InSession,
    /// memory → `RecencyWindow`: recency-active provider.
    RecentIn,
    /// `Skill` → `Skill`, `Fact` → `Fact`: composition / dependency candidates.
    DependsOn,
    /// any → any (polymorphic): erasure cascade; distillation-lineage tracing.
    DerivedFrom,
    /// `Fact`/`CoreBlock` → `Agent`: quorum promotion; attester reliability.
    AttestedBy,
    /// `Fact` → `Fact`: cross-namespace promotion lineage (team→global).
    PromotedTo,
    /// `Fact` → `Fact`: inverse of promotion — lost-support demotion.
    DemotedFrom,
    /// `Skill` → `BadPattern`: Reflexion linkage.
    HasFailure,
    /// `Note` → `Note`: link evolution (scoped).
    RelatesTo,
    /// any memory → `ProvenanceRecord`: resolve a memory to its signed write proof.
    HasProvenance,
    /// `AuditEvent` → any (polymorphic): connect audit events to subjects.
    Audit,
    /// any memory / `WorkItem` → `Tag`: the cross-cutting classification facet.
    HasTag,
}

impl EdgeLabel {
    /// The selene-db relationship name for this label.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            EdgeLabel::Mentions => Mentions::LABEL,
            EdgeLabel::About => About::LABEL,
            EdgeLabel::Supports => Supports::LABEL,
            EdgeLabel::SupersededBy => SupersededBy::LABEL,
            EdgeLabel::Contradicts => Contradicts::LABEL,
            EdgeLabel::ValidAt => ValidAt::LABEL,
            EdgeLabel::InScope => InScope::LABEL,
            EdgeLabel::InSession => InSession::LABEL,
            EdgeLabel::RecentIn => RecentIn::LABEL,
            EdgeLabel::DependsOn => DependsOn::LABEL,
            EdgeLabel::DerivedFrom => DerivedFrom::LABEL,
            EdgeLabel::AttestedBy => AttestedBy::LABEL,
            EdgeLabel::PromotedTo => PromotedTo::LABEL,
            EdgeLabel::DemotedFrom => DemotedFrom::LABEL,
            EdgeLabel::HasFailure => HasFailure::LABEL,
            EdgeLabel::RelatesTo => RelatesTo::LABEL,
            EdgeLabel::HasProvenance => HasProvenance::LABEL,
            EdgeLabel::Audit => Audit::LABEL,
            EdgeLabel::HasTag => HasTag::LABEL,
        }
    }
}

/// `Episode` → `Entity`: an episode mentions an entity (02 §5, bi-temporal).
///
/// Records extraction provenance and feeds entity-anchored retrieval. Bi-temporal:
/// the mention's validity tracks when the episode asserted the entity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mentions {
    /// The four-timestamp validity block.
    pub temporal: BiTemporal,
}

impl Mentions {
    /// The selene-db relationship label for this kind.
    pub const LABEL: &str = "MENTIONS";
}

/// `Fact` → `Entity`: the fact's canonical subject (02 §5, bi-temporal).
///
/// Carries the fact's validity window and drives current-state computation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct About {
    /// The four-timestamp validity block (the fact's validity window).
    pub temporal: BiTemporal,
}

impl About {
    /// The selene-db relationship label for this kind.
    pub const LABEL: &str = "ABOUT";
}

/// `Fact`/`Episode` → `Fact`: one memory supports a fact (02 §5, not bi-temporal).
///
/// Feeds graph-expanded support scoring and the provenance-required current state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Supports {
    /// Support weight contributed to the target fact.
    pub weight: f64,
}

impl Supports {
    /// The selene-db relationship label for this kind.
    pub const LABEL: &str = "SUPPORTS";
}

/// `Fact` → `Fact`: the source fact is superseded by the target (02 §5, bi-temporal).
///
/// Drives the maintained current-state (unsuperseded) provider: a live edge here
/// removes the source from the "what is true now" set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupersededBy {
    /// Why the supersession occurred.
    pub reason: String,
    /// The four-timestamp validity block.
    pub temporal: BiTemporal,
}

impl SupersededBy {
    /// The selene-db relationship label for this kind.
    pub const LABEL: &str = "SUPERSEDED_BY";
}

/// `Fact` → `Fact`: the two facts contradict (02 §5, bi-temporal).
///
/// Drives the unresolved-current / quarantine provider: a live edge here removes
/// the source fact from the current-support set (02 §9 `current_support_facts`),
/// so currentness is modeled by edge presence and `Fact.status` is a redundant
/// scalar mirror of it (GAP-4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contradicts {
    /// What detected the contradiction (detector id / rule).
    pub detected_by: String,
    /// The four-timestamp validity block.
    pub temporal: BiTemporal,
}

impl Contradicts {
    /// The selene-db relationship label for this kind.
    pub const LABEL: &str = "CONTRADICTS";
}

/// `Fact` → `ValidityAnchor`: point-in-time validity anchoring (02 §5, bi-temporal).
///
/// Supports current-valid coverage and point-in-time anchoring queries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidAt {
    /// The four-timestamp validity block.
    pub temporal: BiTemporal,
}

impl ValidAt {
    /// The selene-db relationship label for this kind.
    pub const LABEL: &str = "VALID_AT";
}

/// memory → `Scope`: scope membership (02 §5, not bi-temporal; marker).
///
/// The scope-membership provider — the strongest precision filter — is defined over
/// live edges of this kind. The edge has no extra properties.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InScope;

impl InScope {
    /// The selene-db relationship label for this kind.
    pub const LABEL: &str = "IN_SCOPE";
}

/// `Episode`/`Fact` → `Session`: session membership (02 §5, not bi-temporal; marker).
///
/// Feeds session-scoped retrieval and session-diversity. No extra properties.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InSession;

impl InSession {
    /// The selene-db relationship label for this kind.
    pub const LABEL: &str = "IN_SESSION";
}

/// memory → `RecencyWindow`: recency membership (02 §5, not bi-temporal; marker).
///
/// The recency-active provider is defined over live edges of this kind. No extra
/// properties.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RecentIn;

impl RecentIn {
    /// The selene-db relationship label for this kind.
    pub const LABEL: &str = "RECENT_IN";
}

/// `Skill` → `Skill`, `Fact` → `Fact`: a dependency (02 §5, not bi-temporal; marker).
///
/// Drives skill composition and dependency-derived active candidates. No extra
/// properties.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DependsOn;

impl DependsOn {
    /// The selene-db relationship label for this kind.
    pub const LABEL: &str = "DEPENDS_ON";
}

/// any → any: derivation lineage (02 §5, polymorphic, not bi-temporal).
///
/// Drives the erasure cascade and distillation-lineage tracing. Endpoints are
/// relaxed (polymorphic) per spec §5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DerivedFrom {
    /// When the derivation occurred (immutable transaction-time fact).
    pub derived_at: Timestamp,
}

impl DerivedFrom {
    /// The selene-db relationship label for this kind.
    pub const LABEL: &str = "DERIVED_FROM";
}

/// `Fact`/`CoreBlock` → `Agent`: a signed attestation (02 §5, not bi-temporal).
///
/// Feeds quorum promotion and attester-reliability tracking. Survives soft-forget
/// and is removed only on hard purge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttestedBy {
    /// When the attestation was made (immutable).
    pub attested_at: Timestamp,
    /// The attestation signature over the canonical encoding (immutable, 02 §10).
    pub signature: String,
    /// Optional trust category the attestation applies to.
    pub category: Option<String>,
}

impl AttestedBy {
    /// The selene-db relationship label for this kind.
    pub const LABEL: &str = "ATTESTED_BY";
}

/// `Fact` → `Fact`: cross-namespace promotion lineage (02 §5, bi-temporal).
///
/// Links a fact to the team→global fact it was promoted to. No extra properties.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotedTo {
    /// The four-timestamp validity block.
    pub temporal: BiTemporal,
}

impl PromotedTo {
    /// The selene-db relationship label for this kind.
    pub const LABEL: &str = "PROMOTED_TO";
}

/// `Fact` → `Fact`: inverse of promotion — lost-support demotion (02 §5, bi-temporal).
///
/// Links a fact to the lower-namespace fact it was demoted from. No extra properties.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DemotedFrom {
    /// The four-timestamp validity block.
    pub temporal: BiTemporal,
}

impl DemotedFrom {
    /// The selene-db relationship label for this kind.
    pub const LABEL: &str = "DEMOTED_FROM";
}

/// `Skill` → `BadPattern`: a recorded failure (02 §5, not bi-temporal).
///
/// Reflexion linkage from a skill to the negative pattern observed running it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HasFailure {
    /// When the failure was observed (immutable).
    pub observed_at: Timestamp,
}

impl HasFailure {
    /// The selene-db relationship label for this kind.
    pub const LABEL: &str = "HAS_FAILURE";
}

/// `Note` → `Note`: a scoped, evolving link between notes (02 §5, bi-temporal).
///
/// Carries a free-form relationship label; the validity window tracks link evolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelatesTo {
    /// Free-form label naming the relationship between the two notes.
    pub relationship_label: String,
    /// The four-timestamp validity block.
    pub temporal: BiTemporal,
}

impl RelatesTo {
    /// The selene-db relationship label for this kind.
    pub const LABEL: &str = "RELATES_TO";
}

/// any memory → `ProvenanceRecord`: provenance grounding (02 §5, not bi-temporal; marker).
///
/// Resolves a memory to its signed write proof. No extra properties.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HasProvenance;

impl HasProvenance {
    /// The selene-db relationship label for this kind.
    pub const LABEL: &str = "HAS_PROVENANCE";
}

/// `AuditEvent` → any: audit subject linkage (02 §5, polymorphic, not bi-temporal; marker).
///
/// Connects an audit event to the subject it concerns. Endpoints are relaxed
/// (polymorphic) per spec §5. No extra properties.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Audit;

impl Audit {
    /// The selene-db relationship label for this kind.
    pub const LABEL: &str = "AUDIT";
}

/// any memory / `WorkItem` → `Tag`: a cross-cutting classification label (work-structure
/// design §3, not bi-temporal; marker).
///
/// The horizontal classification axis: a memory or work item points at a `Tag` it carries.
/// Many-to-many and endpoint-wide (every retrievable kind plus `WorkItem` may tag). No extra
/// properties — the edge's presence is the whole signal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HasTag;

impl HasTag {
    /// The selene-db relationship label for this kind.
    pub const LABEL: &str = "HAS_TAG";
}
