//! The recall query and its options (03 §6, §8).

use std::time::Duration;

use aionforge_domain::authz::Principal;
use aionforge_domain::time::Timestamp;

use crate::router::QueryClass;

/// A retrieval request: the query text, the principal asking, and the bundle shape.
///
/// `principal` is the caller-asserted reader identity authorization is applied against
/// (06 §1): a recall surfaces only the global space, the reader's own private namespace,
/// and the teams it belongs to — another agent's private content never surfaces. The
/// text is always bound as a GQL parameter downstream, never interpolated, so hostile
/// query text cannot alter a statement.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallQuery {
    /// The natural-language query.
    pub text: String,
    /// The reader the recall is performed for; gates what may surface via its visible set.
    pub principal: Principal,
    /// The target number of memories in the bundle.
    pub limit: usize,
    /// Tuning knobs; [`RecallOptions::default`] is the usual choice.
    pub options: RecallOptions,
}

impl RecallQuery {
    /// A query for `text` on behalf of `principal`, returning up to `limit` memories with
    /// default options.
    #[must_use]
    pub fn new(text: impl Into<String>, principal: Principal, limit: usize) -> Self {
        Self {
            text: text.into(),
            principal,
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
/// mode. A soft-forgotten memory (a node-level `expired_at`, 05 §2) is out of every
/// mode's default read and retained behind that same flag. The supplied instant is always caller-provided — there is no ambient clock in
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
    /// A status/window view, not a forget bypass: a soft-forgotten memory stays out
    /// even here unless [`RecallOptions::include_expired`] also asks for it (05 §2).
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
    /// soft-forgotten memories at the node level in every mode.
    pub temporal: TemporalMode,
    /// The most memories from a single session allowed to fill the bundle before the
    /// rest spill; spilled memories are appended only if the bundle is under-filled
    /// (03 §6). Zero means no cap.
    pub session_diversity_cap: usize,
    /// A wall-clock budget for the whole recall; exceeding it surfaces as
    /// [`RetrievalError::DeadlineExceeded`](crate::RetrievalError::DeadlineExceeded)
    /// (03 §8). `None` means no deadline.
    pub deadline: Option<Duration>,
    /// The one retention flag (05 §2): include memories carrying a node-level
    /// `expired_at` — soft-forgotten facts and expired episodes alike. Honored in
    /// every temporal mode; the default excludes them everywhere (03 §5).
    pub include_expired: bool,
    /// How many candidates to pull from each signal before fusion. Zero falls back to
    /// the retriever's configured default.
    pub fanout: usize,
    /// Whether this is a sensitive query: every Current-mode fact signal (lexical, the
    /// composed high-precision dense, and its fallback) then reads against
    /// `provenance_current_support_facts` — facts grounded by incoming support and
    /// provenance — instead of `current_support_facts`, so an ungrounded fact never
    /// surfaces (03 §4). Conservative default `false`; automatic sensitivity detection is
    /// deferred to a later milestone.
    pub sensitive: bool,
    /// The caller-supplied "now" the importance and recency re-ranks read against (05 §2,
    /// M5.T01). `None` — the default, and the honest "no clock was provided" state — runs
    /// neither re-rank, leaving the recall byte-identical to a pre-decay one. There is no
    /// ambient clock in the retrieval path, so these signals exist only when the caller
    /// stamps this.
    pub now: Option<Timestamp>,
    /// **Request** to surface system-role memories, which are excluded from default recall
    /// (07 §4, M6.T02). This flag alone is inert: it takes effect only when the injected
    /// [`Authorizer`](aionforge_domain::authz::Authorizer) also grants `may_surface_system`
    /// for the principal, so it is a request the authority must confirm, never a
    /// self-service reveal. The default `false`, and a host must not expose it on an
    /// untrusted surface (the MCP search tool does not). When honored it lifts BOTH
    /// exclusion gates in lockstep — the role gate and the system-namespace visibility gate
    /// — since system content is excluded twice.
    pub include_system: bool,
    /// Whether live episodes that have a newer live `supersedes` claimant may appear in
    /// recall. The default `true` preserves audit/provenance behavior: old evidence can
    /// still surface with `superseded_by` metadata. Set this to `false` for a current-only
    /// episode view that hides replaced raw captures while keeping derived fact history
    /// governed by [`TemporalMode`].
    pub include_superseded: bool,
    /// An OPT-IN absolute relevance floor in `[0, 1]` on the dense cosine similarity: a hit
    /// whose similarity is below the floor — or which has no dense score at all (a
    /// lexical/BM25-only hit) — is dropped, so an unrelated query may legitimately return
    /// empty (P0a). `None` (the default) falls back to the retriever's configured
    /// `min_relevance`, mirroring the `fanout == 0` sentinel; the config default is `0.0`
    /// (off), which leaves recall byte-identical. The floor is defined only against the
    /// dense proxy — the one absolute relevance signal in the pipeline.
    pub min_relevance: Option<f64>,
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
            sensitive: false,
            now: None,
            include_system: false,
            include_superseded: true,
            min_relevance: None,
        }
    }
}
