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
}

impl CaptureError {
    /// Wrap a privacy-filter error by its display form (the filter's error type is a
    /// generic seam, so it is captured as text here).
    pub(crate) fn filter(error: impl std::fmt::Display) -> Self {
        Self::Filter(error.to_string())
    }
}
