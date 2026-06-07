//! Capture-side privacy/injection filtering, recall-side untrusted-data tagging, and
//! the cross-family consolidation guard (07).
//!
//! M1.T02 implements the capture-side [`CaptureFilter`]: configurable redaction of
//! sensitive spans plus detection and stripping of known prompt-injection markers,
//! recorded in `Episode.origin` (02 §6.1). Recall-side untrusted-data tagging and the
//! cross-family consolidation guard land with their milestones (M6, M2).

mod error;
mod filter;

pub use error::SecurityError;
pub use filter::{CaptureFilter, InjectionMarker, MatchValidator, RedactionPattern};
