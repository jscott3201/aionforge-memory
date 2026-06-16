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
//! Drift detection (05 §1, M5.T05) lives here too — identity is what forgetting
//! protects: the [`DriftDetector`] scores each core block's distance between the
//! behavior its baseline was attested over and the namespace's behavior now, built
//! from stored vectors only and skipping (never guessing) whatever it cannot vouch
//! for. The baseline itself is written solely through the attested core-edit path.

mod audit_addr;
mod baseline;
mod cooling;
mod detector;
mod eraser;
mod forgetter;
mod pinning;
mod policy;

pub use audit_addr::substrate_actor;
pub use baseline::DriftBaseline;
pub use cooling::CoolingSweepReport;
pub use detector::{
    BaselineNeed, BlockAssessment, CentroidOutcome, DriftDetector, DriftSweepReport,
    drift_warning_id,
};
pub use eraser::{EraseReport, Eraser, PointErase, ResidualRetention};
pub use forgetter::{
    ForgetDecision, ForgetSweepPage, Forgetter, PointForget, PointUnforget, SpareReason,
};
pub use pinning::{PointPin, PointUnpin, pin, unpin};
pub use policy::{DriftPolicy, ErasurePolicy, ForgettingPolicy};
