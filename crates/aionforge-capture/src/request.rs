//! The raw-event capture request (04 §1).

use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::Role;
use aionforge_domain::time::Timestamp;

/// One raw event to capture: its content plus the writer and session context.
///
/// `captured_at` is the single logical instant the event is recorded at — event
/// time and transaction time coincide on the fast path. Every *stored* timestamp on
/// the path is taken from this field rather than read from the system clock, matching
/// the store's caller-supplied-time convention. Record identifiers are still minted
/// as fresh UUIDv7s (sortable by mint time), the one source of write-time uniqueness.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureRequest {
    /// The raw event body, before privacy filtering.
    pub content: String,
    /// The role of the producing actor.
    pub role: Role,
    /// The authoring agent.
    pub agent_id: Id,
    /// The teams the authoring agent belongs to (06 §1). Asserted by the host; used to authorize a
    /// trusted write to a team namespace. Empty for the common single-agent case; an untrusted
    /// write is confined to the private namespace regardless.
    pub teams: Vec<String>,
    /// The owning session, if any.
    pub session_id: Option<Id>,
    /// When the event is recorded.
    pub captured_at: Timestamp,
    /// Writer provenance and origin context.
    pub writer: WriterContext,
    /// Whether this write is trusted. Untrusted writes are forced into the writer's
    /// private agent namespace regardless of [`CaptureRequest::namespace`] (04 §1,
    /// 07): untrusted content never lands in a shared or global namespace.
    pub trusted: bool,
    /// The requested namespace. Honored only for a trusted write; an untrusted write
    /// is always placed in `agent:<agent_id>`. `None` defaults to that private
    /// namespace as well.
    pub namespace: Option<Namespace>,
    /// A writer-asserted supersession hint (04 §1 step 3): the id of a live episode this
    /// capture replaces. Validated against the writer's writable namespaces and recorded
    /// in `Episode.origin` as evidence for consolidation — capture itself never retires
    /// the target. An invalid hint refuses the whole capture with one collapsed error
    /// for every cause, so the hint is no existence oracle.
    pub supersedes: Option<Id>,
}

/// The writer's provenance and origin context, folded into the provenance record
/// and the episode's `origin` block (02 §6.1).
#[derive(Debug, Clone, PartialEq)]
pub struct WriterContext {
    /// The writer model family (the cross-family consolidation guard compares this).
    pub model_family: String,
    /// The writer model version, if known.
    pub model_version: Option<String>,
    /// The transport the capture arrived on (e.g. `mcp`, `library`).
    pub transport: Option<String>,
    /// A correlating request id, if any.
    pub request_id: Option<String>,
    /// Writer trust at write time; clamped to `[0, 1]` before it is recorded.
    pub trust: f64,
    /// The host-supplied signed-write envelope, present when signed writes are in force
    /// (06 §3). `None` on the unsigned fast path. With the provenance gate active, a
    /// `None` here is a fail-closed rejection — an unsigned write under a signed-write
    /// policy. Ignored entirely when no gate is configured.
    pub signed: Option<SignedProvenance>,
}

/// A host-supplied signed-write envelope (06 §3, M4.T03).
///
/// On a signed-write deployment the host mints the episode (subject) id, signs the
/// canonical `(subject_id, agent_id, captured_at)` payload with its Ed25519 private key,
/// and ships both. The substrate verifies — the private key never enters the process —
/// and, on success, adopts `subject_id` as the episode id so the id the host signed over
/// is exactly the id that is stored. The host owns id allocation on the signed path; the
/// substrate rejects a `subject_id` that collides with an existing episode.
#[derive(Debug, Clone, PartialEq)]
pub struct SignedProvenance {
    /// The episode (subject) id the host minted and signed over.
    pub subject_id: Id,
    /// The base64 Ed25519 signature over
    /// `provenance_payload(subject_id, agent_id, captured_at)`.
    pub signature: String,
}
