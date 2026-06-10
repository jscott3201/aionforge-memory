//! The active-forgetting facade: point forget/unforget and the batch sweep (05 §2,
//! M5.T02).
//!
//! Split out of `lib.rs` (which sits against the file-size cap), mirroring the
//! reliability sweep module. Everything here is **off-cursor** and host-cadence: the
//! host calls on its own schedule with its own clock, the engine never reads one. The
//! `Option<Forgetter>` is the single off-switch — absent, the sweep returns an empty
//! report without touching the graph and the point ops answer
//! [`PointForget::Disabled`] / [`PointUnforget::Disabled`] rather than fabricating a
//! "not found".
//!
//! The sweep enumerates the **all-namespaces L0 spine**, like the reliability sweep:
//! forgetting is substrate maintenance, not a principal-scoped read, and a scoped scan
//! would silently skip another namespace's decayed memories. Each applied forget is
//! audited in the forgotten memory's *own* namespace, so the owning agent sees its
//! history through the scoped audit facade.

use aionforge_domain::contracts::Embedder;
use aionforge_domain::ids::Id;
use aionforge_domain::time::Timestamp;
use aionforge_forget::{ForgetSweepPage, PointForget, PointUnforget};
use aionforge_store::ForgetCursor;

use crate::{EngineError, Memory};

impl<E: Embedder> Memory<E> {
    /// Soft-forget one memory by id, fully gated: a pinned, attested, lineage, or
    /// protected-kind memory is never forgotten, and the outcome names the protection
    /// that held (05 §2). Audited in the memory's own namespace; reversible via
    /// [`Memory::unforget`] until the retention prune.
    ///
    /// # Errors
    /// Returns [`EngineError`] if a store read, probe, or write fails.
    pub fn forget(&self, id: &Id, now: &Timestamp) -> Result<PointForget, EngineError> {
        let Some(forgetter) = &self.forgetter else {
            return Ok(PointForget::Disabled);
        };
        Ok(forgetter.forget(id, now)?)
    }

    /// Reverse a soft-forget by id, restoring the memory into default retrieval (05 §2).
    /// No eligibility gate on the way back — restoring is always safe — but a demotion's
    /// expiry stays refused (governance owns it).
    ///
    /// # Errors
    /// Returns [`EngineError`] if a store read or write fails.
    pub fn unforget(&self, id: &Id, now: &Timestamp) -> Result<PointUnforget, EngineError> {
        let Some(forgetter) = &self.forgetter else {
            return Ok(PointUnforget::Disabled);
        };
        Ok(forgetter.unforget(id, now)?)
    }

    /// Sweep one page of forgetting candidates: evaluate every unexpired `Episode` and
    /// `Fact` against the full eligibility gate, soft-forget the all-axes-low, and tally
    /// (05 §2). The page size is the smaller of `limit` and the policy's batch cap.
    ///
    /// The report's `next` is the watermark to persist and pass as `after` on the next
    /// call; `None` means the scan completed and the next sweep starts a fresh pass. A
    /// resumed walk visits exactly the candidates one uninterrupted scan would — the
    /// cursor is a keyset position, stable under concurrent writes — and re-sweeping
    /// already-forgotten ground is a no-op (expired candidates never re-enter a page).
    ///
    /// # Errors
    /// Returns [`EngineError`] if a read, probe, or write fails. A failure loses at most
    /// the in-flight candidate; everything already applied is committed and idempotent.
    pub fn sweep_forgetting(
        &self,
        after: Option<&ForgetCursor>,
        limit: usize,
        now: &Timestamp,
    ) -> Result<ForgetSweepPage, EngineError> {
        let Some(forgetter) = &self.forgetter else {
            return Ok(ForgetSweepPage::default());
        };
        Ok(forgetter.sweep_page(after, limit, now)?)
    }
}
