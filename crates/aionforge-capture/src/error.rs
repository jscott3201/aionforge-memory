//! The capture-path error space.
//!
//! The capture path fails closed: a privacy-filter, embedder, or store/commit error
//! aborts the capture rather than writing unfiltered, vectorless, or partial state.

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

    /// Embedding was enabled for capture, but the embedder failed or returned no vector. The
    /// episode is not written; hosts that intentionally want vectorless capture disable
    /// embedding in configuration.
    #[error("the capture embedder failed: {0}")]
    Embedder(String),

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

    /// A `system`-role write reached the capture funnel (07 §4, M6.T02). The system role is
    /// reserved for substrate-internal content excluded from default recall, so the funnel refuses
    /// it from EVERY caller, trusted or not: an untrusted caller must not pre-stage content an admin
    /// reveal later surfaces as authentic, and a trusted caller cannot route it into the `system`
    /// namespace this way either, because `system` is never directly writable and `trusted` is
    /// host-asserted. Recorded as a `namespace_denied` audit (reason `system_role_not_capturable`)
    /// before this error is returned; no memory is written.
    #[error("a system-role memory cannot be written through the capture funnel")]
    SystemRoleNotWritable,

    /// Injection-marker excision left only residue (07 §5): the filtered content retained no
    /// substance worth remembering, so storing it would plant a junk episode in recall.
    /// Recorded as a `residue_rejected` audit before this error is returned; no memory is
    /// written.
    #[error("the capture content was only residue after injection-marker excision")]
    ResidueOnly,

    /// The supersedes hint did not name a live episode the writer may supersede (04 §1
    /// step 3). Missing id, soft-forgotten target, and a target outside the writer's
    /// writable namespaces all collapse to this one error — the specific cause is
    /// recorded in a `supersedes_rejected` audit, not exposed, so the hint cannot be
    /// used to probe which ids exist. No memory is written.
    #[error("the capture supersedes hint does not name a live memory this writer may supersede")]
    InvalidSupersedesTarget,
}

impl CaptureError {
    /// Wrap a privacy-filter error by its display form (the filter's error type is a
    /// generic seam, so it is captured as text here).
    pub(crate) fn filter(error: impl std::fmt::Display) -> Self {
        Self::Filter(error.to_string())
    }

    /// Wrap an embedder error by display text. The concrete embedder error type is a generic
    /// seam, so the capture surface keeps the stable failure class and an operator-readable
    /// reason.
    pub(crate) fn embedder(error: impl std::fmt::Display) -> Self {
        Self::Embedder(error.to_string())
    }
}
