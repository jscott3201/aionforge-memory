//! Capture-side privacy/injection filtering, recall-side untrusted-data tagging, and
//! the cross-family consolidation guard (07).
//!
//! M1.T02 implements the capture-side [`CaptureFilter`]: configurable redaction of
//! sensitive spans plus detection and stripping of known prompt-injection markers,
//! recorded in `Episode.origin` (02 §6.1). M6.T01 implements the
//! [`CrossFamilyGuard`]: the pure family comparison and refuse-or-warn decision the
//! engine enforces on every inference-calling consolidation rule (07 §3).
//! Recall-side untrusted-data tagging lands with M6.T02.

mod cross_family;
mod error;
mod filter;

pub use cross_family::{
    CrossFamilyGuard, FamilyVerdict, GuardDecision, GuardMode, GuardReason, family_verdict,
};
pub use error::SecurityError;
pub use filter::{CaptureFilter, InjectionMarker, MatchValidator, RedactionPattern};
