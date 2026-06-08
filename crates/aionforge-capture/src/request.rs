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
}
