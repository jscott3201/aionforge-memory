//! Helper graph-anchor nodes for native graph-retrieval primitives (02 §4.14).
//!
//! These carry the reduced identity block only (no [`crate::blocks::Stats`]) — like
//! the forensic/control kinds (02 §3) they are not retrievable memories. They exist
//! so the engine's maintained candidate-state and graph-expanded scoring (02 §9)
//! apply directly: memories link to them via `IN_SCOPE`, `RECENT_IN`, and `VALID_AT`
//! edges, and the scope-membership / recency-active providers materialize their
//! current members.

use serde::{Deserialize, Serialize};

use crate::blocks::Identity;
use crate::time::Timestamp;

/// A retrieval scope: a project / topic / task / tenant boundary (02 §4.14).
///
/// Memories link to a `Scope` via the `IN_SCOPE` edge; the `scope_membership`
/// provider (02 §9) materializes its current members, the strongest precision
/// filter in scope/topic/task/tenant-bounded retrieval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scope {
    /// Shared identity block.
    pub identity: Identity,
    /// The human-facing name of the scope (e.g. the project or task name).
    pub name: String,
    /// The boundary kind this scope draws — e.g. `project` / `topic` / `task` / `tenant`.
    pub scope_kind: String,
}

impl Scope {
    /// The selene-db node label for this kind.
    pub const LABEL: &str = "Scope";
}

/// A graph-authored recency window (02 §4.14).
///
/// Memories link to a `RecencyWindow` via the `RECENT_IN` edge; the
/// `recency_active` provider (02 §9) materializes its current members for
/// freshness/recency-bounded retrieval. The window is described by a label and an
/// optional time interval; both bounds are open (`None`) when the window is
/// unbounded on that side (e.g. an open-ended "recent" window with only a lower
/// bound, or a rolling window whose bounds the provider derives).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecencyWindow {
    /// Shared identity block.
    pub identity: Identity,
    /// A descriptive label for the window (e.g. `last_24h`, `current_session`).
    pub label: String,
    /// Inclusive lower bound of the window; `None` when unbounded below.
    pub starts_at: Option<Timestamp>,
    /// Exclusive upper bound of the window; `None` when open-ended (still active).
    pub ends_at: Option<Timestamp>,
}

impl RecencyWindow {
    /// The selene-db node label for this kind.
    pub const LABEL: &str = "RecencyWindow";
}

/// A validity reference a fact links to via `VALID_AT` (02 §4.14, §5).
///
/// Used for current-valid coverage and point-in-time anchoring: a `Fact`'s
/// `VALID_AT` edge ties the fact to the instant this anchor names, letting the
/// retrieval layer answer "what was true at time T" by anchoring on a shared
/// reference point. The optional descriptor labels well-known instants (e.g.
/// `now`, `release_v1`) without requiring a separate lookup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidityAnchor {
    /// Shared identity block.
    pub identity: Identity,
    /// The instant this anchor references (the point facts are anchored to). Named
    /// `anchored_at`, not `instant`: `INSTANT` is a reserved temporal keyword in
    /// selene-db's GQL grammar (1.3+), so an `instant` property fails to parse.
    pub anchored_at: Timestamp,
    /// An optional human-facing descriptor for a well-known instant.
    pub label: Option<String>,
}

impl ValidityAnchor {
    /// The selene-db node label for this kind.
    pub const LABEL: &str = "ValidityAnchor";
}
