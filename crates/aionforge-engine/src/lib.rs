//! The Aionforge Memory facade: composes the subsystems, owns lifecycle, and enforces cross-cutting policy.
//!
//! [`Memory`] is the one object a host holds. It wires the capture path and the
//! retrieval path over a shared store and a single embedder, so a caller writes with
//! [`Memory::capture`] and reads with [`Memory::search`] without naming the
//! subsystems. It is generic over the [`Embedder`]
//! seam — the real HTTP client in production, a fake in tests — and fixes the
//! capture-side privacy filter to the security crate's default rule set.
//!
//! The capture and retrieval crates carry the cross-cutting policy this facade relies
//! on: untrusted writes are confined to the writer's private namespace, recall applies
//! namespace authorization, and the recall bundle is deterministic. The consolidation,
//! procedural, trust, and forgetting subsystems join the facade in their milestones.

use std::sync::Arc;

use aionforge_capture::{Capturer, ProvenanceGate};
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
use aionforge_trust::{
    AttestationGate, AuditVerifier, Ed25519Verifier, Promoter, SignedWriteGate, StoreKeyResolver,
    SystemWallClock,
};

use aionforge_domain::ids::Id;

/// The widest sane clock-skew tolerance for signed writes (five minutes, 06 §3). Mirrors the
/// `aionforge-config` ceiling; the engine keeps its own copy because it takes no config
/// dependency.
const MAX_CLOCK_SKEW_TOLERANCE_MS: u64 = 300_000;

pub use aionforge_capture::{
    CaptureConfig, CaptureError, CaptureReceipt, CaptureRequest, CaptureVerdict, EmbeddingOutcome,
    SignedProvenance, WriterContext,
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
pub use aionforge_trust::{
    AttestReceipt, AttestRequest, AuditStatus, CategoryRule, DemotionOutcome, PromotionError,
    PromotionOutcome, PromotionPolicy, ReliabilityError, ReliabilityPolicy, ReliabilityScorer,
};

mod audit;
pub use aionforge_store::{AuditCursor, MAX_AUDIT_PAGE};
pub use audit::{AuditPage, AuditRecord, AuditVerification};

/// How the facade configures the capture and retrieval paths.
#[derive(Debug, Clone, Default)]
pub struct MemoryConfig {
    /// Capture-path tuning.
    pub capture: CaptureConfig,
    /// Retrieval tuning.
    pub retriever: RetrieverConfig,
    /// Signed-write gating (06 §3). Off by default; when `signed_writes` is set the engine
    /// builds an Ed25519 provenance gate over the store's registered agent keys. The host maps
    /// `aionforge-config`'s `SecurityConfig` into this, so the engine takes no config dependency.
    pub security: SecurityGate,
    /// Quorum-promotion policy (06 §4). Off by default; when enabled the engine builds the
    /// attestation/promotion orchestrator. The host maps `aionforge-config`'s `PromotionConfig`
    /// into this [`PromotionPolicy`]. The attestation skew gate reuses
    /// `security.clock_skew_tolerance_ms`.
    pub promotion: PromotionPolicy,
    /// Trust-scoring policy (06 §5). Off by default; when enabled the engine builds the reliability
    /// scorer, which folds the append-only reliability event log into each agent's trust cache. The
    /// host maps `aionforge-config`'s `ReliabilityConfig` into this [`ReliabilityPolicy`], so the
    /// engine takes no config dependency — the same indirection as the promotion policy.
    pub reliability: ReliabilityPolicy,
}

/// The engine's signed-write gating posture (06 §3, M4.T03).
///
/// The host maps `aionforge-config`'s `SecurityConfig` into this struct, so the engine stays
/// free of a config dependency. Default is off, so an unconfigured `Memory` composes its
/// capture path exactly as before — no gate, no crypto, the unsigned fast path.
///
/// `SecurityConfig.redaction` has no counterpart here on purpose: redaction is enforced by the
/// capture privacy filter, not the provenance gate, so the host's `SecurityConfig` ->
/// `SecurityGate` mapping carries only `signed_writes` and `clock_skew_tolerance_ms`.
#[derive(Debug, Clone)]
pub struct SecurityGate {
    /// Whether every capture write must carry a verified Ed25519 provenance signature.
    pub signed_writes: bool,
    /// The clock-skew tolerance in milliseconds for signed writes; validated to lie in
    /// `(0, 300_000]` when `signed_writes` is on.
    pub clock_skew_tolerance_ms: u64,
    /// Whether the substrate signs the audit events it authors (06 §6, M4.T06). Off by
    /// default; when on, the engine provisions seed custody + the keyring anchor at
    /// construction, installs the commit-time signer on the store, and verifies rows on
    /// the audit read facade. The host maps `aionforge-config`'s
    /// `security.sign_audit_events` into this.
    pub sign_audit_events: bool,
    /// The data directory hosting the audit seed file and keyring anchor. Required when
    /// `sign_audit_events` is on (the custody layer refuses relative/unsafe paths).
    pub audit_data_dir: Option<std::path::PathBuf>,
    /// The host-resolved env-custody seed (base64), when the deployment opted into
    /// env-var custody via `audit_key_env`. The engine never reads the environment —
    /// the host resolves the named variable (`Config::resolve_audit_seed`) and maps the
    /// secret in. `None` selects file custody (load-or-mint under `audit_data_dir`).
    pub audit_seed: Option<secrecy::SecretString>,
}

impl Default for SecurityGate {
    fn default() -> Self {
        Self {
            signed_writes: false,
            clock_skew_tolerance_ms: 60_000,
            sign_audit_events: false,
            audit_data_dir: None,
            audit_seed: None,
        }
    }
}

impl SecurityGate {
    /// Check the skew bound when signed writes are on (06 §3). Mirrors the `aionforge-config`
    /// rule so the engine validates its own copy regardless of how the host populated it.
    fn validate(&self) -> Result<(), String> {
        if self.sign_audit_events && self.audit_data_dir.is_none() {
            return Err(
                "security.sign_audit_events requires a data directory for the audit seed \
                 and keyring anchor (set audit_data_dir; a relative or unsafe path is \
                 refused by the custody layer)"
                    .to_string(),
            );
        }
        if self.signed_writes {
            if self.clock_skew_tolerance_ms == 0 {
                return Err(
                    "security.clock_skew_tolerance_ms must be greater than zero when signed \
                     writes are on"
                        .to_string(),
                );
            }
            if self.clock_skew_tolerance_ms > MAX_CLOCK_SKEW_TOLERANCE_MS {
                return Err(
                    "security.clock_skew_tolerance_ms must be at most 300000 (five minutes)"
                        .to_string(),
                );
            }
        }
        Ok(())
    }
}

/// The Aionforge Memory facade over a shared store and an embedder.
pub struct Memory<E> {
    store: Arc<Store>,
    embedder: Arc<E>,
    capturer: Capturer<CaptureFilter, Arc<E>>,
    retriever: HybridRetriever<Arc<E>>,
    authorizer: Arc<dyn Authorizer>,
    /// The attestation/promotion orchestrator, present only when promotion is enabled (06 §4).
    promoter: Option<Promoter>,
    /// The reliability scorer, present only when trust scoring is enabled (06 §5). It folds the
    /// reliability event log into agent and fact trust caches, and backs the refold-first
    /// reliability-demotion sweep.
    reliability_scorer: Option<ReliabilityScorer>,
    /// The audit-signature verifier for the read facade (06 §6, M4.T06). `None` until audit
    /// signing is wired: PR-5g builds it from the keyring when `sign_audit_events` is enabled,
    /// alongside the signer. While `None`, every audit read maps to
    /// [`AuditVerification::NotEnabled`] — the substrate never fabricates a checked verdict.
    audit_verifier: Option<AuditVerifier>,
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
    /// Returns [`EngineError::Config`] if the capture tuning, the promotion policy (an
    /// out-of-range `k`/threshold, an unreachable threshold for that `k`, a non-positive prior,
    /// or — with promotion on — a zero or oversized clock-skew tolerance) is out of range, or
    /// [`EngineError::Filter`] if the default privacy filter fails to compile, which the
    /// security crate's tests guard against.
    pub fn new(
        store: Arc<Store>,
        embedder: E,
        config: MemoryConfig,
        now: &Timestamp,
    ) -> Result<Self, EngineError> {
        Self::with_authorizer(store, embedder, config, Arc::new(DefaultAuthorizer), now)
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
        now: &Timestamp,
    ) -> Result<Self, EngineError> {
        // Front-load all configuration validation, before any subsystem is constructed, so an
        // invalid policy is rejected up front and never interleaves with a side-effecting build
        // step. `validate_promotion_skew` covers the promotion-on / signed-writes-off case that
        // `SecurityGate::validate` (gated on `signed_writes`) does not.
        config.capture.validate().map_err(EngineError::Config)?;
        config.security.validate().map_err(EngineError::Config)?;
        config.promotion.validate().map_err(EngineError::Config)?;
        config.reliability.validate().map_err(EngineError::Config)?;
        if config.promotion.enabled {
            validate_promotion_skew(config.security.clock_skew_tolerance_ms)?;
        }
        let embedder = Arc::new(embedder);
        let filter = CaptureFilter::with_defaults().map_err(EngineError::filter)?;
        let capturer = Capturer::new(
            Arc::clone(&store),
            filter,
            Arc::clone(&embedder),
            config.capture,
            Arc::clone(&authorizer),
        );
        // Signed writes (06 §3): when on, gate the capture path with an Ed25519 provenance gate
        // over the store's registered agent keys. This is the single place crypto meets the
        // capture path; the capturer itself stays crypto-free. Off ⇒ no gate, unsigned fast path.
        let capturer = if config.security.signed_writes {
            let gate: Arc<dyn ProvenanceGate> = Arc::new(SignedWriteGate::new(
                Ed25519Verifier,
                Arc::new(StoreKeyResolver::new(Arc::clone(&store))),
                Arc::new(SystemWallClock),
                config.security.clock_skew_tolerance_ms,
            ));
            capturer.with_gate(gate)
        } else {
            capturer
        };
        // Quorum promotion (06 §4): when on, build the attestation/promotion orchestrator over the
        // store's registered agent keys, reusing the signed-write skew knob. The policy and skew
        // were validated up front; the `Option<Promoter>` is the single off-switch, so the engine
        // never invokes the orchestrator while disabled. Off ⇒ no orchestrator, API inert.
        let promoter = if config.promotion.enabled {
            let gate = AttestationGate::new(
                Ed25519Verifier,
                Arc::new(StoreKeyResolver::new(Arc::clone(&store))),
                Arc::new(SystemWallClock),
                config.security.clock_skew_tolerance_ms,
            );
            Some(Promoter::new(
                Arc::clone(&store),
                gate,
                config.promotion.clone(),
            ))
        } else {
            None
        };
        // Trust scoring (06 §5): when on, build the reliability scorer over the store. It writes only
        // off-cursor (folding the reliability event log into the trust caches), so it takes no gate
        // and no clock. The `Option<ReliabilityScorer>` is the single off-switch, mirroring the
        // promoter — off ⇒ every reliability facade method is inert.
        let reliability_scorer = if config.reliability.enabled {
            Some(ReliabilityScorer::new(
                Arc::clone(&store),
                config.reliability.clone(),
            ))
        } else {
            None
        };
        // Substrate audit signing (06 §6, M4.T06): provision custody + the keyring anchor,
        // install the commit-time signer on the store, and keep the keyring-anchored
        // verifier for the read facade — one branch, one off-switch. The genesis event is
        // committed through the audit write funnel: content-addressed, so a replay dedups
        // to a no-op and a genesis-crash window heals (the 5d protocol). This is the only
        // place `sign_audit_events` is read.
        let audit_verifier = if config.security.sign_audit_events {
            let data_dir = config
                .security
                .audit_data_dir
                .as_deref()
                .expect("validated above: sign_audit_events requires audit_data_dir");
            let provision = aionforge_trust::provision_audit_signing(
                data_dir,
                config.security.audit_seed.as_ref(),
                now,
            )
            .map_err(|err| EngineError::AuditSigning(Box::new(err)))?;
            store
                .install_audit_signer(Arc::new(provision.signer))
                .map_err(EngineError::Store)?;
            if let Some(genesis_event) = &provision.genesis_event {
                store.commit_audit(genesis_event)?;
            }
            Some(provision.verifier)
        } else {
            None
        };
        let retriever = HybridRetriever::with_authorizer(
            Arc::clone(&store),
            Arc::clone(&embedder),
            config.retriever,
            Arc::clone(&authorizer),
        );
        Ok(Self {
            store,
            embedder,
            capturer,
            retriever,
            authorizer,
            promoter,
            reliability_scorer,
            audit_verifier,
        })
    }

    /// The namespace authority this memory is governed by — the single seam a host overrides
    /// through [`Memory::with_authorizer`]. It gates writes (a capture must be authorized for
    /// its target namespace) and reads alike (a recall surfaces only the principal's visible
    /// set), so one injected policy bounds the whole memory (06 §1).
    #[must_use]
    pub fn authorizer(&self) -> &Arc<dyn Authorizer> {
        &self.authorizer
    }

    /// The audit-signature verifier, when audit signing is enabled (M4.T06).
    pub(crate) fn audit_verifier(&self) -> Option<&AuditVerifier> {
        self.audit_verifier.as_ref()
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
        Self::new(Arc::new(store), embedder, config, now)
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

    /// Record a signed attestation of a fact and evaluate it for promotion (06 §4).
    ///
    /// Explicit-only: the attester must already know `request.fact_id` — there is no surface that
    /// lists pending candidates. When promotion is off the call is inert (a receipt with
    /// `recorded = false`). A refused attestation is audited and surfaces a coarse error.
    ///
    /// # Errors
    /// Returns [`EngineError::Promotion`] for a rejected attestation, a clock-skew rejection, a
    /// key-resolution backend fault, or a store failure.
    pub fn attest(&self, request: AttestRequest) -> Result<AttestReceipt, EngineError> {
        match &self.promoter {
            Some(promoter) => Ok(promoter.attest(&request)?),
            None => Ok(AttestReceipt {
                recorded: false,
                promoted: None,
            }),
        }
    }

    /// Evaluate a team fact for quorum promotion, promoting it when the reliability-weighted
    /// posterior clears the per-category threshold with enough distinct attesters (06 §4).
    ///
    /// `now` is the caller's clock — the facade keeps no ambient clock, so the promotion's stored
    /// transaction time is deterministic. Idempotent. Returns [`PromotionOutcome::Disabled`] when
    /// promotion is off.
    ///
    /// # Errors
    /// Returns [`EngineError::Promotion`] if a store read or the write fails.
    pub fn evaluate_promotion(
        &self,
        fact_id: &Id,
        now: &Timestamp,
    ) -> Result<PromotionOutcome, EngineError> {
        match &self.promoter {
            Some(promoter) => Ok(promoter.evaluate_promotion(fact_id, now)?),
            None => Ok(PromotionOutcome::Disabled),
        }
    }

    /// Evaluate a promoted candidate for demotion on lost support: when the team original has
    /// dropped out of the current-support set, quarantine the global copy and leave the namespace
    /// original untouched (06 §4). `now` is the caller's clock. Idempotent. Returns
    /// [`DemotionOutcome::Disabled`] when promotion is off.
    ///
    /// # Errors
    /// Returns [`EngineError::Promotion`] if a store read or the write fails.
    pub fn evaluate_demotion(
        &self,
        candidate_fact_id: &Id,
        now: &Timestamp,
    ) -> Result<DemotionOutcome, EngineError> {
        match &self.promoter {
            Some(promoter) => Ok(promoter.evaluate_demotion(candidate_fact_id, now)?),
            None => Ok(DemotionOutcome::Disabled),
        }
    }

    /// Refold the reliability caches for a set of agents from the committed event log: each agent's
    /// trust scores and its produced facts' trust are recomputed (06 §5). Off-cursor and idempotent
    /// — a no-move refold writes nothing. Does nothing and returns `Ok(())` when trust scoring is
    /// off.
    ///
    /// # Errors
    /// Returns [`EngineError::Reliability`] if a store read or a cache write fails.
    pub fn refold_reliability(&self, agents: &[Id]) -> Result<(), EngineError> {
        let Some(scorer) = &self.reliability_scorer else {
            return Ok(());
        };
        for agent in agents {
            scorer.refold_agent(agent)?;
        }
        Ok(())
    }

    /// Sweep a set of promoted candidates for reliability-decay demotion (06 §5). For each candidate
    /// this refolds its attesters' reliability **first**, then evaluates the reliability-demotion
    /// gate — honoring the scorer's refold-first contract, so the recomputed posterior reads fresh
    /// reliability and the verdict is not arrival-order dependent. The refolded set is exactly the
    /// attester set the gate reads, so the two never disagree.
    ///
    /// Reliability demotion needs both halves on: the scorer to refold and the promoter to evaluate.
    /// With either off, every candidate reports [`DemotionOutcome::Disabled`]; an unknown candidate
    /// reports [`DemotionOutcome::NoChange`]. `now` is the caller's clock.
    ///
    /// # Errors
    /// Returns [`EngineError::Store`] if a candidate or attester read fails,
    /// [`EngineError::Reliability`] if a refold fails, or [`EngineError::Promotion`] if the demotion
    /// read or write fails.
    pub fn sweep_reliability_demotions(
        &self,
        candidates: &[Id],
        now: &Timestamp,
    ) -> Result<Vec<DemotionOutcome>, EngineError> {
        let (Some(scorer), Some(promoter)) = (&self.reliability_scorer, &self.promoter) else {
            return Ok(vec![DemotionOutcome::Disabled; candidates.len()]);
        };
        let mut outcomes = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let Some(team_node) = self.store.fact_node_by_id(candidate)? else {
                outcomes.push(DemotionOutcome::NoChange);
                continue;
            };
            // Refold-first: refresh each attester from the committed event log before the gate reads
            // its cache.
            for attester in self.store.distinct_attesters(team_node)? {
                scorer.refold_agent(&attester.attester_id)?;
            }
            outcomes.push(promoter.evaluate_reliability_demotion(candidate, now)?);
        }
        Ok(outcomes)
    }

    /// Record a producer decay (trigger D1): each distinct producing agent of a contradicted,
    /// quarantined `victim_fact` takes a reliability decay, folded into its trust cache (06 §5).
    /// Host-driven and off-cursor. Returns the number of decay events recorded — `0` when trust
    /// scoring is off or the fact is unknown. Idempotent: a replay records nothing new.
    ///
    /// # Errors
    /// Returns [`EngineError::Store`] if the fact-resolution read fails, or
    /// [`EngineError::Reliability`] if the scorer's read or its off-cursor cache write fails.
    pub fn record_reliability_decay(
        &self,
        victim_fact: &Id,
        now: &Timestamp,
    ) -> Result<usize, EngineError> {
        let Some(scorer) = &self.reliability_scorer else {
            return Ok(0);
        };
        let Some(node) = self.store.fact_node_by_id(victim_fact)? else {
            return Ok(0);
        };
        let events = scorer.quarantine_decay(node, now)?;
        scorer.apply(&events)?;
        Ok(events.len())
    }

    /// Record an attester decay (trigger D2): each distinct attester of a `demoted_fact` takes a
    /// reliability decay in the fact's category (06 §5) — the symmetric partner of
    /// [`Memory::record_reliability_decay`] for the demotion side. Host-driven and off-cursor.
    /// Returns the number of decay events recorded — `0` when off or the fact is unknown. Idempotent.
    ///
    /// # Errors
    /// Returns [`EngineError::Store`] if the fact-resolution read fails, or
    /// [`EngineError::Reliability`] if the scorer's read or its off-cursor cache write fails.
    pub fn record_reliability_demotion(
        &self,
        demoted_fact: &Id,
        now: &Timestamp,
    ) -> Result<usize, EngineError> {
        let Some(scorer) = &self.reliability_scorer else {
            return Ok(0);
        };
        let Some(node) = self.store.fact_node_by_id(demoted_fact)? else {
            return Ok(0);
        };
        let events = scorer.demotion_decay(node, now)?;
        scorer.apply(&events)?;
        Ok(events.len())
    }

    /// Record an agreement gain (trigger G1): when a later, distinct-authored `corroborating_fact`
    /// carries what an earlier `asserted_fact`'s producers claimed, each of those producers earns a
    /// reliability gain (06 §5). The scorer's distinct-author guard drops self-corroboration.
    /// Host-driven and off-cursor. Returns the number of gain events recorded — `0` when off, either
    /// fact is unknown, or the guard dropped them all. Idempotent.
    ///
    /// # Errors
    /// Returns [`EngineError::Store`] if the fact-resolution read fails, or
    /// [`EngineError::Reliability`] if the scorer's read or its off-cursor cache write fails.
    pub fn record_reliability_agreement(
        &self,
        asserted_fact: &Id,
        corroborating_fact: &Id,
        now: &Timestamp,
    ) -> Result<usize, EngineError> {
        let Some(scorer) = &self.reliability_scorer else {
            return Ok(0);
        };
        let (Some(asserted), Some(corroborating)) = (
            self.store.fact_node_by_id(asserted_fact)?,
            self.store.fact_node_by_id(corroborating_fact)?,
        ) else {
            return Ok(0);
        };
        let events = scorer.agreement_gain(asserted, corroborating, now)?;
        scorer.apply(&events)?;
        Ok(events.len())
    }
}

/// Validate the clock-skew tolerance the attestation gate reuses when promotion is on (06 §4),
/// mirroring the signed-write bound. When signed writes are also on, [`SecurityGate::validate`]
/// has already checked it; this covers promotion-on-but-signed-writes-off so a zero or oversized
/// window is a configuration error, not a silent lockout.
fn validate_promotion_skew(tolerance_ms: u64) -> Result<(), EngineError> {
    if tolerance_ms == 0 {
        return Err(EngineError::Config(
            "security.clock_skew_tolerance_ms must be greater than zero when promotion is on"
                .to_string(),
        ));
    }
    if tolerance_ms > MAX_CLOCK_SKEW_TOLERANCE_MS {
        return Err(EngineError::Config(
            "security.clock_skew_tolerance_ms must be at most 300000 (five minutes)".to_string(),
        ));
    }
    Ok(())
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

    /// Audit-signing provisioning failed at startup (custody, keyring anchor, or a seed
    /// that does not match the anchored keyring). Signing was requested, so this is fatal.
    #[error("audit signing could not be provisioned")]
    AuditSigning(#[source] Box<aionforge_trust::ProvisionError>),

    /// The optional off-cursor LLM distiller failed (a store read, embedding, or the write).
    #[error("distillation failed")]
    Distillation(#[from] DistillError),

    /// The optional off-cursor LLM link evolver failed (a store read or the write).
    #[error("link evolution failed")]
    LinkEvolution(#[from] LinkEvolveError),

    /// The optional attestation/quorum-promotion path refused an attestation or failed (06 §4).
    #[error("attestation or promotion failed")]
    Promotion(#[from] PromotionError),

    /// The optional trust-scoring path failed (a store read or an off-cursor cache write, 06 §5).
    #[error("reliability scoring failed")]
    Reliability(#[from] ReliabilityError),

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
