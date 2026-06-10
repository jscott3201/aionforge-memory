//! The right-to-erasure facade (05 §3, M5.T03).
//!
//! Split out of `lib.rs` (which sits against the file-size cap), mirroring the
//! forgetting facade. Two gates stand in front of the cascade, in order: the
//! `Option<Eraser>` off-switch (its own `erasure.enabled`, deliberately separate from
//! `forgetting.enabled` — the reversible sweep and the one destructive path are
//! separate authorities), and the engine's injected namespace authority. Erasure is
//! the one principal-driven surface on the forgetting side — the sweep and the point
//! forget are substrate maintenance, but an erase destroys on an agent's say-so — so
//! the caller supplies the [`Principal`], the
//! [`Authorizer`](aionforge_domain::authz::Authorizer) rules on every namespace the
//! cascade spans, and the purge audit names that principal as its actor.

use aionforge_domain::authz::Principal;
use aionforge_domain::contracts::Embedder;
use aionforge_domain::ids::Id;
use aionforge_domain::time::Timestamp;
use aionforge_forget::PointErase;

use crate::{EngineError, Memory};

impl<E: Embedder> Memory<E> {
    /// Erase one memory and its derivation cascade by id (05 §3): irreversible,
    /// audited, fully reported. The principal must hold write authority over every
    /// namespace the cascade spans, or the whole erasure is refused untouched — and
    /// under the default policy that means no plain principal erases global or system
    /// ground. No forgetting protection gates it: erase succeeds on a pinned or
    /// attested memory by design, because those gates spare from the *reversible*
    /// sweep and this is the escalation they defer to.
    ///
    /// # Errors
    /// Returns [`EngineError`] if a store read, walk, or write fails. Every refusal —
    /// disabled, not found, unauthorized, over-cap — is a typed [`PointErase`],
    /// decided before any write.
    pub fn erase(
        &self,
        principal: &Principal,
        id: &Id,
        now: &Timestamp,
    ) -> Result<PointErase, EngineError> {
        let Some(eraser) = &self.eraser else {
            return Ok(PointErase::Disabled);
        };
        Ok(eraser.erase(principal, self.authorizer.as_ref(), id, now)?)
    }
}
