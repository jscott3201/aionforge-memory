//! Shared audit addressing for the M5 lifecycle transitions (05 §2).
//!
//! Forget/unforget and pin/unpin record their decisions through the same addressing
//! discipline, lifted here so the two surfaces can never drift apart: cycle-addressed
//! event ids (a subject legitimately cycles through a transition more than once, and
//! each crossing is a distinct row), identities in the **memory's own namespace**
//! (agent-visible through the scoped audit reads, never hidden in `System`
//! governance forensics), and one deterministic substrate actor.

use aionforge_domain::blocks::Identity;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::time::Timestamp;

/// The deterministic substrate actor recorded on lifecycle audits — sweep-driven and
/// manual alike, until a caller principal is plumbed through the facade.
pub(crate) fn substrate_actor() -> Id {
    Id::from_content_hash(b"aionforge/forgetter-v1")
}

/// A content-addressed id over `(tag, subject)` plus the event's millisecond instant —
/// the governance-transition discipline (`system_audit::cycle_id` precedent): a subject
/// legitimately cycles forget → unforget → forget (or pin → unpin → pin), and each
/// transition is a distinct row. Sound because emission is gated on a real state flip,
/// and crash-safe because a replay re-supplies the same host `now`.
pub(crate) fn cycle_id(tag: &str, subject: &Id, now: &Timestamp) -> Id {
    let millis = now.timestamp().as_millisecond();
    Id::from_content_hash(format!("{tag}|{subject}|{millis}").as_bytes())
}

/// The audit identity for a lifecycle event: addressed to the **memory's own
/// namespace** — agent-visible, never `System` — which is the one deliberate divergence
/// from the `system_audit` helper (the engine's audit read facade filters on the event's
/// own namespace, and a governance-namespace row would hide an agent's own history).
pub(crate) fn namespace_identity(id: Id, namespace: Namespace, now: &Timestamp) -> Identity {
    Identity {
        id,
        ingested_at: now.clone(),
        namespace,
        expired_at: None,
    }
}
