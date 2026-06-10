//! Active forgetting and the M5 lifecycle surfaces (05 §2).
//!
//! This crate owns the forgetting orchestrator: the conservative, default-off,
//! reversible soft-expiry that composes the domain's pure eligibility axes with the
//! store's graph probes and write primitives. The engine holds a [`Forgetter`] only when
//! the policy enables it, mirroring the promotion and reliability components — absent
//! means off, and every forgetting facade surface is inert. The pin/unpin point ops are
//! the deliberate exception: always available regardless of that switch, because the
//! pin's first consumer is read-time decay (which runs with forgetting off) and a pin
//! can only ever spare a memory.
//!
//! Erasure cascade, attested core memory, and drift detection (M5.T03–T05) will join
//! this crate as they land.

mod audit_addr;
mod forgetter;
mod pinning;
mod policy;

pub use forgetter::{
    ForgetDecision, ForgetSweepPage, Forgetter, PointForget, PointUnforget, SpareReason,
};
pub use pinning::{PointPin, PointUnpin, pin, unpin};
pub use policy::ForgettingPolicy;
