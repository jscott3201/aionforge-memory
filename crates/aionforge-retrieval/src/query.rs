//! The recall query and its options (03 §6, §8).

use std::time::Duration;

use aionforge_domain::namespace::Namespace;
use aionforge_domain::time::Timestamp;

use crate::router::QueryClass;

/// A retrieval request: the query text, the namespace asking, and the bundle shape.
///
/// `viewer` is the namespace authorization is applied against — private content from
/// another agent never surfaces to it (03 §8, 06 §1). The text is always bound as a
/// GQL parameter downstream, never interpolated, so hostile query text cannot alter a
/// statement.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallQuery {
    /// The natural-language query.
    pub text: String,
    /// The namespace the recall is performed for; gates what may surface.
    pub viewer: Namespace,
    /// The target number of memories in the bundle.
    pub limit: usize,
    /// Tuning knobs; [`RecallOptions::default`] is the usual choice.
    pub options: RecallOptions,
}

impl RecallQuery {
    /// A query for `text` on behalf of `viewer`, returning up to `limit` memories with
    /// default options.
    #[must_use]
    pub fn new(text: impl Into<String>, viewer: Namespace, limit: usize) -> Self {
        Self {
            text: text.into(),
            viewer,
            limit,
            options: RecallOptions::default(),
        }
    }
}

/// Which slice of bi-temporal history a recall reads its facts against (03 §5).
///
/// Currentness in this substrate is modeled by edge presence, not a flag: a fact is
/// current iff it has no live `SUPERSEDED_BY` and no live `CONTRADICTS` edge (the
/// `current_support_facts` provider, 02 §9). The two closed windows on a fact's
/// `ABOUT` edge let a caller ask two independent questions — what was true in the
/// world at an instant (*event time*, `valid_from`/`valid_to`) versus what the
/// substrate believed at an instant (*transaction time*, `ingested_at`/`expired_at`).
///
/// This mode shapes only **facts**; episodes are raw turns with no validity window, so
/// they are gated by [`RecallOptions::include_expired`] instead and surface in every
/// mode. The supplied instant is always caller-provided — there is no ambient clock in
/// the retrieval path (a deterministic-recall requirement, 03 §6).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TemporalMode {
    /// What is true now: facts with no live supersession/contradiction and an active
    /// status. The conservative default and the high-precision factual path (03 §4).
    #[default]
    Current,
    /// What was true in the world at an instant (event time): keep a fact whose
    /// `valid_from <= t` and whose `valid_to` is open or after `t`.
    AsOf(Timestamp),
    /// What the substrate believed at an instant (transaction time): keep a fact whose
    /// `ingested_at <= t` and whose `expired_at` is open or after `t`.
    AsKnownAt(Timestamp),
    /// The whole record: every status and window, including superseded, contradicted,
    /// and quarantined facts. The explicit opt-in for an audit/history view (03 §5).
    History,
}

/// Optional retrieval tuning (03 §3, §5, §6, §8).
#[derive(Debug, Clone, PartialEq)]
pub struct RecallOptions {
    /// Force a query class instead of letting the router classify (mostly for tests
    /// and callers that already know the intent).
    pub mode_override: Option<QueryClass>,
    /// Which bi-temporal slice to read facts against; defaults to
    /// [`TemporalMode::Current`] (03 §5). Orthogonal to `include_expired`, which gates
    /// soft-forgotten episodes only.
    pub temporal: TemporalMode,
    /// The most memories from a single session allowed to fill the bundle before the
    /// rest spill; spilled memories are appended only if the bundle is under-filled
    /// (03 §6). Zero means no cap.
    pub session_diversity_cap: usize,
    /// A wall-clock budget for the whole recall; exceeding it surfaces as
    /// [`RetrievalError::DeadlineExceeded`](crate::RetrievalError::DeadlineExceeded)
    /// (03 §8). `None` means no deadline.
    pub deadline: Option<Duration>,
    /// Include soft-forgotten (expired) memories — a history query. The default
    /// current retrieval excludes them (03 §5).
    pub include_expired: bool,
    /// How many candidates to pull from each signal before fusion. Zero falls back to
    /// the retriever's configured default.
    pub fanout: usize,
}

impl Default for RecallOptions {
    fn default() -> Self {
        Self {
            mode_override: None,
            temporal: TemporalMode::default(),
            session_diversity_cap: 3,
            deadline: None,
            include_expired: false,
            fanout: 0,
        }
    }
}
