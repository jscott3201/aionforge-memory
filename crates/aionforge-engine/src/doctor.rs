//! Engine-level doctor report that adds live embedder identity to store health.

use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::EmbedderModel;
use aionforge_store::{StoreDoctorReport, VectorDimensionMismatch};

use crate::{EngineError, Memory, telemetry};

/// Live embedder health as seen by the engine facade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedderDoctorReport {
    /// Declared model identity for the embedder backing capture/retrieval/consolidation.
    pub model: EmbedderModel,
    /// Embedding dimension carried by the store configuration.
    pub store_config_dimension: u32,
    /// True when the live embedder dimension matches the store configuration.
    pub matches_store_config: bool,
    /// Vector indexes whose pinned dimension differs from the live embedder.
    pub vector_dimension_mismatches: Vec<VectorDimensionMismatch>,
    /// True when the live embedder matches both store config and vector indexes.
    pub ok: bool,
}

/// The canonical engine doctor snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryDoctorReport {
    /// True when every store and embedder check represented in this report is healthy.
    pub ok: bool,
    /// Store-level schema, index, provider, lag, and graph occupancy health.
    pub store: StoreDoctorReport,
    /// Live embedder identity and dimension health.
    pub embedder: EmbedderDoctorReport,
}

impl<E: Embedder> Memory<E> {
    /// Build the read-only engine doctor report.
    ///
    /// Health failures are represented in the returned report (`ok = false`) rather than
    /// as errors. This returns [`EngineError`] only when the underlying store cannot be queried.
    pub fn doctor_report(&self) -> Result<MemoryDoctorReport, EngineError> {
        let store = self.store.doctor_report()?;
        let embedder = self.embedder_doctor_report();
        let ok = store.ok && embedder.ok;
        telemetry::doctor_report(&store, ok);
        Ok(MemoryDoctorReport {
            ok,
            store,
            embedder,
        })
    }

    fn embedder_doctor_report(&self) -> EmbedderDoctorReport {
        let model = self.embedder.model().clone();
        let store_config_dimension = self.store.config().embedding_dimension;
        let vector_dimension_mismatches = self
            .store
            .vector_indexes()
            .into_iter()
            .filter(|index| index.dimension != model.dimension)
            .map(|index| VectorDimensionMismatch {
                label: index.label,
                property: index.property,
                expected_dimension: model.dimension,
                actual_dimension: index.dimension,
            })
            .collect::<Vec<_>>();
        let matches_store_config = store_config_dimension == model.dimension;
        let ok = matches_store_config && vector_dimension_mismatches.is_empty();
        EmbedderDoctorReport {
            model,
            store_config_dimension,
            matches_store_config,
            vector_dimension_mismatches,
            ok,
        }
    }
}
