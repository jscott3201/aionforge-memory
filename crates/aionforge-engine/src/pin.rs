//! The pin/unpin facade (05 §2, M5.T02 rider).
//!
//! Split out of `lib.rs` (which sits against the file-size cap), mirroring the
//! forgetting facade — with one deliberate difference: there is **no off-switch**.
//! Pin and unpin do not sit behind the `Option<Forgetter>` because the pin's first
//! consumer is read-time decay, which runs whether or not active forgetting is enabled,
//! and because a pin can only ever spare a memory from a sweep, never doom one. An
//! agent under the shipped default configuration (forgetting off) can still pin a
//! hard-won memory so neither decay nor a later-enabled sweep ages it out.
//!
//! Both ops are audited in the memory's own namespace through the same cycle-addressed
//! discipline as forget/unforget, so the scoped audit reads show an agent its own
//! pin history.

use aionforge_domain::contracts::Embedder;
use aionforge_domain::ids::Id;
use aionforge_domain::time::Timestamp;
use aionforge_forget::{PointPin, PointUnpin};

use crate::{EngineError, Memory};

impl<E: Embedder> Memory<E> {
    /// Pin one memory by id: hold it at full write-time importance in every ranking and
    /// spare it from every forgetting path (05 §2). Works on every `Stats`-bearing
    /// kind, on any status, and on a soft-forgotten memory (without restoring it —
    /// [`Memory::unforget`] stays its own transition). Audited; reversible via
    /// [`Memory::unpin`].
    ///
    /// # Errors
    /// Returns [`EngineError`] if a store read or write fails.
    pub fn pin(&self, id: &Id, now: &Timestamp, actor: &Id) -> Result<PointPin, EngineError> {
        Ok(aionforge_forget::pin(&self.store, id, now, actor)?)
    }

    /// Lift a pin by id (05 §2). A pin is a stay, not a vault: the memory re-enters
    /// decay and sweep eligibility, and is forgotten later only if every eligibility
    /// axis independently holds low. Audited.
    ///
    /// # Errors
    /// Returns [`EngineError`] if a store read or write fails.
    pub fn unpin(&self, id: &Id, now: &Timestamp, actor: &Id) -> Result<PointUnpin, EngineError> {
        Ok(aionforge_forget::unpin(&self.store, id, now, actor)?)
    }
}
