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
use aionforge_consolidate::{
    Consolidator, Distiller, FactExtractionPass, LinkEvolvePass, SkillInductionPass,
};
use aionforge_domain::contracts::{
    Capture, Embedder, FactExtractor, LinkEvolver, Retriever, SkillInducer, Summarizer,
};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::time::Timestamp;
use aionforge_retrieval::HybridRetriever;
use aionforge_security::{CaptureFilter, SecurityError};

pub use aionforge_capture::{
    CaptureConfig, CaptureReceipt, CaptureRequest, CaptureVerdict, EmbeddingOutcome, WriterContext,
};
pub use aionforge_consolidate::{
    ConsolidationConfig, ConsolidationHandle, ConsolidationLag, DISTILL_RULE_VERSION,
    DetectionConfig, DistillError, DistillationConfig, DistillationReport, InductionConfig,
    LINK_EVOLVE_RULE_VERSION, LLMLinkEvolver, LLMSummarizer, LinkEvolveConfig, LinkEvolveError,
    LinkEvolveReport, ObjectRule, PassConfig, PredicateRule, RELATIONSHIP_VOCABULARY,
    RULE_LINK_EVOLVE_VERSION, ResolutionConfig, Rule, RuleExtractor, RuleInducer, RuleLinkEvolver,
    RuleSummarizer, SummarizationConfig,
};
pub use aionforge_domain::authz::{
    AuthorizationError, Authorizer, DefaultAuthorizer, DenyReason, Principal, VisibleSet,
};
pub use aionforge_retrieval::{
    EpisodeEntry, FactEntry, QueryClass, RecallBundle, RecallExplanation, RecallOptions,
    RecallQuery, RetrieverConfig, Signal, SignalWeights, StructuredEntry, TemporalMode,
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
    authorizer: Arc<dyn Authorizer>,
}

impl<E: Embedder> Memory<E> {
    /// Build a memory over an already-migrated store and an embedder, with the default namespace
    /// authority ([`DefaultAuthorizer`]).
    ///
    /// The one embedder backs both the capture and retrieval paths through a shared
    /// reference, so the client is built once. The capture-side privacy filter uses
    /// the security crate's conservative default patterns.
    ///
    /// # Errors
    /// Returns [`EngineError::Config`] if the capture tuning is out of range, or
    /// [`EngineError::Filter`] if the default privacy filter fails to compile, which the
    /// security crate's tests guard against.
    pub fn new(store: Arc<Store>, embedder: E, config: MemoryConfig) -> Result<Self, EngineError> {
        Self::with_authorizer(store, embedder, config, Arc::new(DefaultAuthorizer))
    }

    /// Build a memory with an explicit namespace authority — the injection point for a stricter
    /// policy (e.g. signature-gated writes in M4.T03) behind the same [`Authorizer`] seam.
    ///
    /// # Errors
    /// As [`Memory::new`].
    pub fn with_authorizer(
        store: Arc<Store>,
        embedder: E,
        config: MemoryConfig,
        authorizer: Arc<dyn Authorizer>,
    ) -> Result<Self, EngineError> {
        config.capture.validate().map_err(EngineError::Config)?;
        let embedder = Arc::new(embedder);
        let filter = CaptureFilter::with_defaults().map_err(EngineError::filter)?;
        let capturer = Capturer::new(
            Arc::clone(&store),
            filter,
            Arc::clone(&embedder),
            config.capture,
            Arc::clone(&authorizer),
        );
        let retriever =
            HybridRetriever::new(Arc::clone(&store), Arc::clone(&embedder), config.retriever);
        Ok(Self {
            store,
            embedder,
            capturer,
            retriever,
            authorizer,
        })
    }

    /// The namespace authority every capture write is checked against — the single seam a host
    /// overrides through [`Memory::with_authorizer`]. Recall scopes its own reads by namespace
    /// today; folding that read filter onto this authority is a later step.
    #[must_use]
    pub fn authorizer(&self) -> &Arc<dyn Authorizer> {
        &self.authorizer
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

    /// The current consolidation backlog, resolved against `now`.
    ///
    /// Lets a host observe "capture-to-derived" lag and the pending/failed episode counts
    /// for health and SLA checks without reaching into the store (L0). `now` is injected —
    /// the facade keeps no ambient clock — so the lag is deterministic and matches whatever
    /// instant the caller is reasoning about. Works whether or not a consolidator is running.
    ///
    /// # Errors
    /// Returns [`EngineError::Store`] if the backlog query fails.
    pub fn consolidation_lag(&self, now: &Timestamp) -> Result<ConsolidationLag, EngineError> {
        let snapshot = self.store.consolidation_lag()?;
        Ok(ConsolidationLag::from_snapshot(&snapshot, now))
    }
}

impl<E: Embedder + 'static> Memory<E> {
    /// Start the background consolidator with the fact-extraction pass (04 §2, M2.T04).
    ///
    /// This is opt-in and explicit so `Memory::new` stays synchronous and runtime-free:
    /// a host that wants slow consolidation calls this from inside a Tokio runtime and
    /// holds the returned [`ConsolidationHandle`] for the process lifetime, shutting it
    /// down on exit.
    ///
    /// Exactly one consolidator may run against a given store. The single consolidation
    /// cursor and the atomic state flips assume one writer of derived memory; starting a
    /// second loop on the same store — in this process or another that shares it — can
    /// double-process episodes or stall the cursor, and is unsupported. The loop is not
    /// re-entrant: ticks run one at a time and a tick that overruns its interval is skipped,
    /// never overlapped, so a slow pass throttles throughput rather than racing itself.
    ///
    /// The pass shares this memory's embedder, so derived entities, facts,
    /// and notes are embedded with the same model as capture and retrieval. The injected
    /// [`FactExtractor`], [`Summarizer`], and [`SkillInducer`] are the deterministic
    /// [`RuleExtractor`] / [`RuleSummarizer`] / [`RuleInducer`] in tests and the model-backed
    /// clients in production (M4 / the optional M3.S3 distillation layer).
    ///
    /// Skill induction is registered as a second pass but is **off unless
    /// `pass_config.induction.enabled`** is set; a disabled pass is skipped by the scheduler and
    /// absent from the cursor, so the default posture is extraction-only.
    pub fn start_consolidation<X, Sz, I>(
        &self,
        extractor: X,
        summarizer: Sz,
        inducer: I,
        config: ConsolidationConfig,
        pass_config: PassConfig,
    ) -> ConsolidationHandle
    where
        X: FactExtractor + 'static,
        Sz: Summarizer + 'static,
        I: SkillInducer + 'static,
    {
        let induction_config = pass_config.induction.clone();
        let extraction = FactExtractionPass::new(
            Arc::new(extractor),
            Arc::clone(&self.embedder),
            Arc::new(summarizer),
            pass_config,
        );
        let induction = SkillInductionPass::new(Arc::new(inducer), induction_config);
        let mut consolidator = Consolidator::new(Arc::clone(&self.store), config);
        consolidator.register(Box::new(extraction));
        consolidator.register(Box::new(induction));
        consolidator.start()
    }

    /// Run the optional, off-by-default LLM distiller over one namespace's current support facts,
    /// **off the consolidation cursor** (M3.T08, 04 §*Canonical vs. distilled*).
    ///
    /// This is the on-demand entry point for distillation — call it at session end, on a timer, or
    /// from a tool. It is independent of [`start_consolidation`](Self::start_consolidation): the
    /// scheduler keeps writing the canonical, byte-deterministic rule summaries inside the cursor
    /// flip, while this condenses the same subjects with the injected model-backed
    /// [`Summarizer`] (an [`LLMSummarizer`] over the chat client) into non-canonical
    /// `DERIVED_FROM`-linked notes that sit alongside canonical recall and never enter the
    /// current-fact path. The summarizer is injected rather than constructed here, so the facade
    /// stays off the chat-client crate and a caller chooses (and gates) the model.
    ///
    /// A no-op (empty report) unless `config.enabled` is set. A slow or unavailable model degrades
    /// to the canonical tier — each such call is recorded and writes no note — so distillation can
    /// never stall or corrupt the cursor. `now` is supplied by the caller; the facade keeps no
    /// ambient clock, so distilled-note transaction time is deterministic.
    ///
    /// The caller is responsible for populating `config.endpoint` and `config.seed` from the same
    /// completer configuration that built `summarizer` — they are recorded in each call's
    /// provenance audit (the `Summarizer` seam does not expose them), never used to drive behavior.
    ///
    /// # Errors
    /// Returns [`EngineError::Distillation`] if a store read, the note-body embedding, or the
    /// final write fails. A model that is unavailable or returns nothing usable is not an error.
    pub async fn distill<Sz>(
        &self,
        summarizer: Sz,
        namespace: &Namespace,
        config: DistillationConfig,
        now: &Timestamp,
    ) -> Result<DistillationReport, EngineError>
    where
        Sz: Summarizer,
    {
        let distiller = Distiller::new(summarizer, Arc::clone(&self.embedder), config);
        let report = distiller.distill(&self.store, namespace, now).await?;
        Ok(report)
    }

    /// Evolve the live notes of one namespace into non-canonical `RELATES_TO` links with the
    /// injected [`LinkEvolver`], off the consolidation cursor (M3.T09).
    ///
    /// A no-op (empty report) unless `config.enabled` is set. A slow or unavailable model degrades
    /// to the deterministic rule tier — each such call is recorded and writes no edge — so link
    /// evolution can never stall or corrupt the cursor. Unlike [`Self::distill`] this needs no
    /// embedder: the evolver scores the notes' already-stored embeddings. `now` is supplied by the
    /// caller; the facade keeps no ambient clock, so link transaction time is deterministic.
    ///
    /// The caller is responsible for populating `config.endpoint` and `config.seed` from the same
    /// completer configuration that built `evolver` — they are recorded in each call's provenance
    /// audit (the `LinkEvolver` seam does not expose them), never used to drive behavior.
    ///
    /// # Errors
    /// Returns [`EngineError::LinkEvolution`] if a store read or the final write fails. A model that
    /// is unavailable or returns nothing usable is not an error.
    pub async fn evolve_links<Lv>(
        &self,
        evolver: Lv,
        namespace: &Namespace,
        config: LinkEvolveConfig,
        now: &Timestamp,
    ) -> Result<LinkEvolveReport, EngineError>
    where
        Lv: LinkEvolver,
    {
        let pass = LinkEvolvePass::new(evolver, config);
        let report = pass.evolve_links(&self.store, namespace, now).await?;
        Ok(report)
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

    /// The optional off-cursor LLM distiller failed (a store read, embedding, or the write).
    #[error("distillation failed")]
    Distillation(#[from] DistillError),

    /// The optional off-cursor LLM link evolver failed (a store read or the write).
    #[error("link evolution failed")]
    LinkEvolution(#[from] LinkEvolveError),

    /// The default capture privacy filter could not be built.
    #[error("could not initialize the capture filter: {0}")]
    Filter(String),

    /// The facade configuration is out of range.
    #[error("invalid memory configuration: {0}")]
    Config(String),
}

impl EngineError {
    /// Wrap a security-filter construction error as text (the security error type is a
    /// separate crate's seam).
    fn filter(error: SecurityError) -> Self {
        Self::Filter(error.to_string())
    }
}
