//! Shared audit addressing for the M5 lifecycle transitions (05 §2-§3).
//!
//! Forget/unforget, pin/unpin, and the eraser's purge audit record their decisions
//! through the same addressing discipline, lifted here so the surfaces can never drift
//! apart: one fresh, time-ordered id per **applied** transition, identities in the
//! **memory's own namespace** (agent-visible through the scoped audit reads, never
//! hidden in `System` governance forensics). The actor differs by surface: the
//! substrate actor below for the **sweep-driven** flip, the **acting agent** for a manual
//! point-op (pin/unpin/forget/unforget) and for the eraser's purge — destruction or a
//! deliberate lifecycle change on an agent's say-so is attributed to the agent (P1).

use aionforge_domain::blocks::Identity;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::time::Timestamp;

/// The deterministic substrate actor recorded on the **sweep-driven** forget audit — the
/// flip no agent asked for. The manual point-ops (pin/unpin/forget/unforget) and the
/// eraser's purge instead name the acting `principal.agent_id`: a lifecycle change made on
/// an agent's say-so is attributed to the agent, not the substrate (P1). Exposed so a
/// reader can recognize a substrate-authored audit row (and so tests can assert the sweep
/// uses it).
#[must_use]
pub fn substrate_actor() -> Id {
    Id::from_content_hash(b"aionforge/forgetter-v1")
}

/// A fresh id for one applied transition — every real state flip is its own audit row,
/// even `pin → unpin → pin` inside a single millisecond.
///
/// Deliberately **generated, not content-addressed**. Idempotency does not live in the
/// id: the store writes flip-and-audit atomically and emit the audit only on a real
/// state transition, so a crash-retry or double call is a state-gated no-op that never
/// builds an event at all. A content hash over `(tag, subject, instant)` — the earlier
/// shape — added nothing to that guarantee and *cost* a real defect: a subject crossing
/// the same transition twice within one millisecond collided into one id, and the
/// second crossing committed with its audit row silently deduplicated away, leaving a
/// history whose last row contradicted the node's state.
pub(crate) fn transition_id() -> Id {
    Id::generate()
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
