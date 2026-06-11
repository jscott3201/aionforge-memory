//! The capture receipt: what the capture path decided and wrote (04 §1).

use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::Redaction;

/// The outcome of capturing one event.
///
/// On an exact-duplicate the receipt names the existing episode and writes nothing
/// new ([`CaptureVerdict::ExactDuplicate`], `audit_id` is `None`); otherwise it
/// carries the freshly assigned episode and audit ids and what the filter did.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureReceipt {
    /// The episode's stable domain id — the newly committed one, or the existing one
    /// an exact-duplicate mapped to.
    pub episode_id: Id,
    /// The dedup verdict.
    pub verdict: CaptureVerdict,
    /// The capture audit event's domain id, or `None` when nothing was written
    /// (an exact duplicate).
    pub audit_id: Option<Id>,
    /// The namespace the episode was actually placed in, after trust enforcement
    /// (an untrusted write is forced private). Lets a caller see a downgrade.
    pub namespace: Namespace,
    /// The redactions applied to the content on the capture path.
    pub redactions: Vec<Redaction>,
    /// Ids of prompt-injection markers flagged in the content.
    pub injection_flags: Vec<String>,
    /// Whether the content was embedded, skipped, or not requested.
    pub embedding: EmbeddingOutcome,
    /// The validated supersedes hint recorded in the episode's origin (04 §1 step 3),
    /// echoed so the writer can confirm the claim landed. `None` when no hint was sent
    /// — and on an exact duplicate, where nothing new is written and the existing
    /// episode's immutable origin cannot adopt the claim.
    pub supersedes: Option<Id>,
}

/// The dedup decision the capture path made (04 §1 step 2–3).
#[derive(Debug, Clone, PartialEq)]
pub enum CaptureVerdict {
    /// A new, distinct episode was committed.
    New,
    /// The content hash matched a live episode; nothing was written.
    ExactDuplicate,
    /// The content was committed but is similar to an existing episode above the
    /// configured threshold. Episodes are immutable, so it is still written; the
    /// similarity is surfaced for consolidation to reconcile.
    NearDuplicate {
        /// The existing episode it resembles.
        similar_to: Id,
        /// The cosine distance to that episode (smaller is more similar).
        distance: f64,
    },
}

/// Whether the capture path produced an embedding for the episode.
#[derive(Debug, Clone, PartialEq)]
pub enum EmbeddingOutcome {
    /// The content was embedded and the vector stored on the episode.
    Embedded,
    /// Embedding was attempted but failed; the episode was written without a vector
    /// and will be embedded during consolidation (§8.1 graceful degradation). The
    /// string is the failure reason, kept for observability.
    Skipped(String),
    /// Embedding on capture is disabled by configuration.
    NotRequested,
}
