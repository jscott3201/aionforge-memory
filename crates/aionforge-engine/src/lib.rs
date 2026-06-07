//! The Aionforge Memory facade: composes the subsystems, owns lifecycle, and enforces cross-cutting policy.
//!
//! [`Memory`] is the one object a host holds. It wires the capture path and the
//! retrieval path over a shared store and a single embedder, so a caller writes with
//! [`Memory::capture`] and reads with [`Memory::search`] without naming the
//! subsystems. It is generic over the [`Embedder`](aionforge_domain::contracts::Embedder)
//! seam — the real HTTP client in production, a fake in tests — and fixes the
//! capture-side privacy filter to the security crate's default rule set.
//!
//! The capture and retrieval crates carry the cross-cutting policy this facade relies
//! on: untrusted writes are confined to the writer's private namespace, recall applies
//! namespace authorization, and the recall bundle is deterministic. The consolidation,
//! procedural, trust, and forgetting subsystems join the facade in their milestones.

use std::sync::Arc;

use aionforge_capture::Capturer;
use aionforge_consolidate::{Consolidator, FactExtractionPass};
use aionforge_domain::contracts::{Capture, Embedder, FactExtractor, Retriever};
use aionforge_domain::time::Timestamp;
use aionforge_retrieval::HybridRetriever;
use aionforge_security::{CaptureFilter, SecurityError};

pub use aionforge_capture::{
    CaptureConfig, CaptureReceipt, CaptureRequest, CaptureVerdict, EmbeddingOutcome, WriterContext,
};
pub use aionforge_consolidate::{
    ConsolidationConfig, ConsolidationHandle, DetectionConfig, ObjectRule, PassConfig,
    PredicateRule, ResolutionConfig, Rule, RuleExtractor,
};
pub use aionforge_retrieval::{
    QueryClass, RecallBundle, RecallExplanation, RecallOptions, RecallQuery, RetrieverConfig,
    Signal, SignalWeights, StructuredEntry,
};
pub use aionforge_store::{Store, StoreConfig};

/// How the facade configures the capture and retrieval paths.
#[derive(Debug, Clone, Default)]
pub struct MemoryConfig {
    /// Capture-path tuning.
    pub capture: CaptureConfig,
    /// Retrieval tuning.
    pub retriever: RetrieverConfig,
}

/// The Aionforge Memory facade over a shared store and an embedder.
pub struct Memory<E> {
    store: Arc<Store>,
    embedder: Arc<E>,
    capturer: Capturer<CaptureFilter, Arc<E>>,
    retriever: HybridRetriever<Arc<E>>,
}

impl<E: Embedder> Memory<E> {
    /// Build a memory over an already-migrated store and an embedder.
    ///
    /// The one embedder backs both the capture and retrieval paths through a shared
    /// reference, so the client is built once. The capture-side privacy filter uses
    /// the security crate's conservative default patterns.
    ///
    /// # Errors
    /// Returns [`EngineError::Filter`] if the default privacy filter fails to compile,
    /// which the security crate's tests guard against.
    pub fn new(store: Arc<Store>, embedder: E, config: MemoryConfig) -> Result<Self, EngineError> {
        let embedder = Arc::new(embedder);
        let filter = CaptureFilter::with_defaults().map_err(EngineError::filter)?;
        let capturer = Capturer::new(
            Arc::clone(&store),
            filter,
            Arc::clone(&embedder),
            config.capture,
        );
        let retriever =
            HybridRetriever::new(Arc::clone(&store), Arc::clone(&embedder), config.retriever);
        Ok(Self {
            store,
            embedder,
            capturer,
            retriever,
        })
    }

    /// Open an in-memory memory: a fresh store sized to the embedder's dimension,
    /// migrated as of `now`, then wired up. The convenient way to stand a memory up
    /// for a host or a test without managing the store directly.
    ///
    /// # Errors
    /// Returns [`EngineError::Store`] if the store cannot be opened or migrated, or
    /// [`EngineError::Filter`] if the default privacy filter fails to compile.
    pub fn open_in_memory(
        embedder: E,
        now: &Timestamp,
        config: MemoryConfig,
    ) -> Result<Self, EngineError> {
        let store = Store::open_with_config(StoreConfig {
            embedding_dimension: embedder.model().dimension,
        })?;
        store.migrate(now)?;
        Self::new(Arc::new(store), embedder, config)
    }

    /// Capture one event on the fast path (04 §1).
    ///
    /// # Errors
    /// Returns [`EngineError::Capture`] if filtering or the commit fails.
    pub async fn capture(&self, request: CaptureRequest) -> Result<CaptureReceipt, EngineError> {
        Ok(self.capturer.capture(request).await?)
    }

    /// Run a retrieval, returning a deterministic recall bundle (03 §6).
    ///
    /// # Errors
    /// Returns [`EngineError::Retrieval`] if a search fails or the deadline is exceeded.
    pub async fn search(&self, query: RecallQuery) -> Result<RecallBundle, EngineError> {
        Ok(self.retriever.recall(query).await?)
    }

    /// The shared store, for lifecycle and inspection.
    #[must_use]
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// The shared embedder backing capture, retrieval, and consolidation.
    #[must_use]
    pub fn embedder(&self) -> &Arc<E> {
        &self.embedder
    }
}

impl<E: Embedder + 'static> Memory<E> {
    /// Start the background consolidator with the fact-extraction pass (04 §2, M2.T04).
    ///
    /// This is opt-in and explicit so `Memory::new` stays synchronous and runtime-free:
    /// a host that wants slow consolidation calls this from inside a Tokio runtime and
    /// holds the returned [`ConsolidationHandle`] for the process lifetime, shutting it
    /// down on exit. The pass shares this memory's embedder, so derived entities and
    /// facts are embedded with the same model as capture and retrieval. The injected
    /// [`FactExtractor`] is the deterministic [`RuleExtractor`] in tests and the
    /// model-backed client in production (M4).
    pub fn start_consolidation<X>(
        &self,
        extractor: X,
        config: ConsolidationConfig,
        pass_config: PassConfig,
    ) -> ConsolidationHandle
    where
        X: FactExtractor + 'static,
    {
        let pass =
            FactExtractionPass::new(Arc::new(extractor), Arc::clone(&self.embedder), pass_config);
        let mut consolidator = Consolidator::new(Arc::clone(&self.store), config);
        consolidator.register(Box::new(pass));
        consolidator.start()
    }
}

/// An error from the memory facade.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[non_exhaustive]
pub enum EngineError {
    /// Opening or migrating the store failed.
    #[error("the store operation failed")]
    Store(#[from] aionforge_store::StoreError),

    /// The capture path failed.
    #[error("capture failed")]
    Capture(#[from] aionforge_capture::CaptureError),

    /// The retrieval path failed.
    #[error("retrieval failed")]
    Retrieval(#[from] aionforge_retrieval::RetrievalError),

    /// The default capture privacy filter could not be built.
    #[error("could not initialize the capture filter: {0}")]
    Filter(String),
}

impl EngineError {
    /// Wrap a security-filter construction error as text (the security error type is a
    /// separate crate's seam).
    fn filter(error: SecurityError) -> Self {
        Self::Filter(error.to_string())
    }
}
