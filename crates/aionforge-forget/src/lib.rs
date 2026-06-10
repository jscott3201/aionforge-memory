//! Active forgetting and the M5 lifecycle surfaces (05 §2).
//!
//! This crate owns the forgetting orchestrator: the conservative, default-off,
//! reversible soft-expiry that composes the domain's pure eligibility axes with the
//! store's graph probes and write primitives. The engine holds a [`Forgetter`] only when
//! the policy enables it, mirroring the promotion and reliability components — absent
//! means off, and every facade surface is inert.
//!
//! Erasure cascade, attested core memory, and drift detection (M5.T03–T05) will join
//! this crate as they land.

mod forgetter;
mod policy;

pub use forgetter::{
    ForgetDecision, ForgetSweepPage, Forgetter, PointForget, PointUnforget, SpareReason,
};
pub use policy::ForgettingPolicy;
