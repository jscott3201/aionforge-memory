//! The procedural-memory error space.
//!
//! Saving a skill fails closed on a missing problem embedding it cannot compute: a skill the
//! vector index can never surface is a silent retrieval defect, and saves are not on the hot
//! path, so an embedder outage aborts the save rather than storing a half-retrievable skill.
//! Retrieval, by contrast, degrades — a down embedder falls back to the lexical signal — so a
//! query-time embedder outage is not in this space.

/// An error from the procedural-memory layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProceduralError {
    /// A store read, write, or translation failed.
    #[error(transparent)]
    Store(#[from] aionforge_store::StoreError),

    /// Computing a skill's problem embedding failed and the caller supplied none, so the skill
    /// could not be made retrievable by vector. Fail closed rather than store a skill the vector
    /// index can never surface.
    #[error("embedding the skill problem failed: {0}")]
    Embed(String),

    /// No skill exists with the referenced domain id (e.g. recording an outcome against an id
    /// that was never saved).
    #[error("no skill found with id {0}")]
    NotFound(String),
}
