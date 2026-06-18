//! Pin / unpin point ops (05 §2, M5.T02 rider) — the surface that makes the pin real.
//!
//! Every write path stamps `is_pinned: false`, so until this surface no memory could
//! ever *become* pinned and the pin protections (the decay short-circuit, the forgetting
//! spare) were unreachable. These ops are **always available** — deliberately not behind
//! the forgetting off-switch, because the pin's first consumer is read-time decay, which
//! runs regardless of whether active forgetting is enabled, and because a pin can only
//! ever spare a memory, never doom one. They are free functions rather than `Forgetter`
//! methods for the same reason: the `Option<Forgetter>` is `None` under the default
//! configuration, and the pin must not be.
//!
//! Scope is every `Stats`-bearing kind: pin works on everything that has a pin field.
//! Pinning a `CoreBlock` is redundant (identity memory is hard-exempt from the sweep)
//! but harmless, and one uniform rule beats a second admission table. A soft-forgotten
//! memory may be pinned without restoring it — the pin protects it while it stays out
//! of default recall, and un-forgetting remains its own audited transition. A pin is a
//! **stay, not a vault**: lifting it re-arms decay and sweep eligibility silently, and
//! the memory is forgotten later only if every eligibility axis independently holds low.

use aionforge_domain::ids::Id;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::Timestamp;
use aionforge_store::{ForgetCandidate, PinWrite, Store, StoreError};

use crate::audit_addr::{namespace_identity, transition_id};
use crate::forgetter::ALL_MEMORY_LABELS;

/// The outcome of a point-pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointPin {
    /// Pinned and audited.
    Pinned,
    /// Already pinned; nothing changed, nothing audited.
    AlreadyPinned,
    /// No memory carries this id.
    NotFound,
}

/// The outcome of a point-unpin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointUnpin {
    /// The pin was lifted and audited; decay and sweep eligibility re-arm.
    Unpinned,
    /// Not pinned in the first place; nothing changed, nothing audited.
    NotPinned,
    /// No memory carries this id.
    NotFound,
}

/// Pin one memory by id (05 §2). Resolves over every `Stats`-bearing kind; no
/// eligibility gate, no status gate — a pin can only spare.
///
/// `actor` is the acting agent recorded on the audit row: pin/unpin are manual-only
/// surfaces (there is no sweep that pins), so a pin is always attributable to the agent
/// that asked for it — the same "on an agent's say-so" attribution the eraser's purge
/// audit uses, never the substrate actor.
///
/// # Errors
/// Returns [`StoreError`] if a read or write fails.
pub fn pin(store: &Store, id: &Id, now: &Timestamp, actor: &Id) -> Result<PointPin, StoreError> {
    let Some(candidate) = store.memory_by_id(id, &ALL_MEMORY_LABELS)? else {
        return Ok(PointPin::NotFound);
    };
    let audit = pin_audit(&candidate, now, AuditKind::Pin, "manual_pin", actor);
    match store.set_pinned(candidate.node, &audit)? {
        PinWrite::Applied => Ok(PointPin::Pinned),
        PinWrite::Noop => Ok(PointPin::AlreadyPinned),
    }
}

/// Lift a pin by id (05 §2). The reverse write is as ungated as the forward one;
/// the unpin audit row is the durable record the stay was lifted.
///
/// # Errors
/// Returns [`StoreError`] if a read or write fails.
pub fn unpin(
    store: &Store,
    id: &Id,
    now: &Timestamp,
    actor: &Id,
) -> Result<PointUnpin, StoreError> {
    let Some(candidate) = store.memory_by_id(id, &ALL_MEMORY_LABELS)? else {
        return Ok(PointUnpin::NotFound);
    };
    let audit = pin_audit(&candidate, now, AuditKind::Unpin, "manual_unpin", actor);
    match store.clear_pinned(candidate.node, &audit)? {
        PinWrite::Applied => Ok(PointUnpin::Unpinned),
        PinWrite::Noop => Ok(PointUnpin::NotPinned),
    }
}

/// The pin/unpin audit event: one fresh row per applied transition, in the memory's
/// own namespace, attributed to the acting `actor`, with the terse reason-and-kind
/// payload (the unforget shape — there is no decision basis to explain, because there is
/// no decision gate).
fn pin_audit(
    candidate: &ForgetCandidate,
    now: &Timestamp,
    kind: AuditKind,
    reason: &str,
    actor: &Id,
) -> AuditEvent {
    AuditEvent {
        identity: namespace_identity(transition_id(), candidate.identity.namespace.clone(), now),
        kind,
        subject_id: candidate.identity.id,
        actor_id: *actor,
        payload: serde_json::json!({
            "reason": reason,
            "kind": candidate.label,
        }),
        signature: String::new(),
        occurred_at: now.clone(),
    }
}
