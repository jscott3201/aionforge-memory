//! The note-lineage read facade (07 §3, M6.T01).
//!
//! Split out of `lib.rs` (which sits against the file-size cap). One read: the
//! "lineage and model identity queryable via `DERIVED_FROM` + provenance" acceptance
//! surface for distilled notes — sources, producing model, writer families, and the
//! structural non-canonical marker — assembled by the store from records that all
//! already exist. Always available: lineage answers "where did this note come from",
//! which must stay readable whether or not the guard (or distillation itself) is
//! enabled.

use aionforge_domain::contracts::Embedder;
use aionforge_domain::ids::Id;
use aionforge_store::NoteLineage;

use crate::{EngineError, Memory};

impl<E: Embedder> Memory<E> {
    /// A note's lineage bundle: its source facts and episodes (via `DERIVED_FROM`),
    /// the model that authored it (from its `Distill` audit, `None` for a rule
    /// summary), and the writer families behind its sources — or `None` when no
    /// live note carries the id (07 §T3, M6.T01).
    ///
    /// A point read; callers wanting to filter notes *by* model family should not
    /// scan lineage (the producing model is decoded from audit payload, not an
    /// indexed column).
    ///
    /// # Errors
    /// Returns [`EngineError`] if a store read or decode fails.
    pub fn note_lineage(&self, note_id: &Id) -> Result<Option<NoteLineage>, EngineError> {
        Ok(self.store.note_lineage(note_id)?)
    }
}
