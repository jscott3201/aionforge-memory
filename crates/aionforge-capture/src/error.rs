//! The capture-path error space.
//!
//! The capture path fails closed: a privacy-filter error or a store/commit error
//! aborts the capture rather than writing unfiltered or partial state. An embedding
//! failure is *not* in this space — it degrades to a vector-less write recorded on
//! the receipt (04 §1, §8.1), so capture never blocks on the embedder.

/// An error from the capture path.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[non_exhaustive]
pub enum CaptureError {
    /// The privacy/injection filter failed; the content is not written unfiltered.
    #[error("the capture privacy filter failed: {0}")]
    Filter(String),

    /// A store read, the commit, or a translation failed.
    #[error("the capture store operation failed")]
    Store(#[from] aionforge_store::StoreError),

    /// The write was refused by namespace authorization: the agent is not permitted to write the
    /// target namespace (06 §1). The attempt is recorded as a `namespace_denied` audit before this
    /// error is returned.
    #[error("the capture write was not authorized: {0}")]
    Unauthorized(#[from] aionforge_domain::authz::AuthorizationError),

    /// The provenance signature was rejected under a signed-write policy (06 §3): the writer
    /// is not registered, the signature did not verify, the write was unsigned, or its
    /// host-supplied subject id collided with an existing episode. The specific cause is
    /// recorded in an `invalid_signature` audit, not exposed here, so the substrate is neither
    /// an enrollment oracle nor a forge oracle. No memory is written.
    #[error("the capture provenance signature was rejected")]
    InvalidSignature,

    /// The write's timestamp deviates from the substrate clock beyond the configured bound
    /// (06 §3, replay/storm mitigation). Recorded as a `clock_skew_rejected` audit before this
    /// error is returned; no memory is written.
    #[error(
        "the capture timestamp is {skew_ms}ms off the substrate clock, beyond the {tolerance_ms}ms bound"
    )]
    ClockSkew {
        /// The absolute deviation between the write timestamp and the substrate clock.
        skew_ms: i64,
        /// The configured tolerance the deviation exceeded.
        tolerance_ms: u64,
    },

    /// The provenance gate could not resolve the writer's key because a backend read failed.
    /// An availability fault, not a rejection: no security audit is written and the write is
    /// not attributed to an attacker.
    #[error("the provenance gate is unavailable: {0}")]
    ProvenanceUnavailable(String),
}

impl CaptureError {
    /// Wrap a privacy-filter error by its display form (the filter's error type is a
    /// generic seam, so it is captured as text here).
    pub(crate) fn filter(error: impl std::fmt::Display) -> Self {
        Self::Filter(error.to_string())
    }
}
