//! Subsystem contract traits — the type-only seams between Aionforge's subsystems.
//!
//! The domain crate is the one crate every other crate depends on, so the
//! cross-cutting subsystem contracts live here: declaring them centrally lets any
//! layer name a seam without inducing a dependency cycle. These are forward
//! declarations. Each names a subsystem's primary operation and its fallible,
//! mostly-async shape; where a request/response is not yet expressible in domain
//! terms it is an associated type the implementing milestone defines, so nothing
//! here invents a persisted surface ahead of the milestone that owns it.
//!
//! Async methods are written `-> impl Future<Output = …> + Send` rather than
//! `async fn` so the returned future's `Send` bound is explicit (required by the
//! multi-threaded Tokio runtime) and the public-`async-fn`-in-trait lint stays
//! quiet under `-D warnings`.

use std::future::Future;

use crate::embedding::{EmbedderModel, Embedding};
use crate::ids::Id;
use crate::nodes::associative::Note;
use crate::nodes::episodic::{Episode, Redaction};
use crate::nodes::procedural::{RankedSkill, Skill};
use crate::nodes::semantic::{Fact, SourceSpan};
use crate::value::ObjectValue;

/// The fast, ADD-oriented capture path (04 §1). Implemented in M1.
pub trait Capture: Send + Sync {
    /// The raw-event capture request (content plus writer/session context).
    type Request: Send;
    /// The capture receipt (assigned ids, dedup verdict, audit reference).
    type Receipt: Send;
    /// The typed error this seam surfaces.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Capture one event on the fast path; never blocks on consolidation (04 §1).
    fn capture(
        &self,
        request: Self::Request,
    ) -> impl Future<Output = Result<Self::Receipt, Self::Error>> + Send;
}

/// The composed, query-class-conditional retrieval operation (03). Implemented in M1.
pub trait Retriever: Send + Sync {
    /// The retrieval query (text, mode weights, bi-temporal selector, deadline).
    type Query: Send;
    /// The recall bundle: coordinated structured and rendered views (03 §6).
    type Bundle: Send;
    /// The typed error this seam surfaces.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Run a retrieval, returning a deterministic recall bundle (03 §6).
    fn recall(
        &self,
        query: Self::Query,
    ) -> impl Future<Output = Result<Self::Bundle, Self::Error>> + Send;
}

/// The slow, asynchronous, durable consolidation path (04 §2). Implemented in M2.
pub trait Consolidator: Send + Sync {
    /// A summary of one pass: rules applied, cursor advance, observed lag.
    type Report: Send;
    /// The typed error this seam surfaces.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Advance the durable cursor by one bounded, idempotent pass (04 §2–§3).
    fn advance(&self) -> impl Future<Output = Result<Self::Report, Self::Error>> + Send;
}

/// Procedural memory: skills stored as data and their reliability (05). Implemented in M3.
pub trait ProceduralMemory: Send + Sync {
    /// The typed error this seam surfaces.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Save a new skill version (deprecate-never-delete), returning its id (05).
    fn save_skill(&self, skill: Skill) -> impl Future<Output = Result<Id, Self::Error>> + Send;

    /// Record a success/failure outcome against a skill, updating its counters (05).
    fn record_outcome(
        &self,
        skill_id: Id,
        success: bool,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Record a failure against a skill *and* remember why: bump its failure counter and store a
    /// linked [`BadPattern`](crate::nodes::procedural::BadPattern) describing the failure mode,
    /// returning the pattern's id (05). The companion to [`Self::record_outcome`] for failures
    /// worth remembering — a known failure mode resurfaces with the skill so it is visible before
    /// reuse and weighs the skill down when the current problem looks like it.
    fn record_failure(
        &self,
        skill_id: Id,
        description: String,
    ) -> impl Future<Output = Result<Id, Self::Error>> + Send;

    /// Retrieve the active skills whose stored problem best matches `problem`, reliability-
    /// weighted and best-first, at most `k` (05).
    ///
    /// A dedicated procedural-recall entry point — separate from the episodic/fact recall bundle
    /// ([`Retriever::recall`]) — because skill selection ranks on a different axis: problem match
    /// re-weighted by proven reliability, not bi-temporal relevance. Only live, active versions
    /// surface; deprecated and soft-forgotten versions are history.
    fn retrieve_skills(
        &self,
        problem: String,
        k: usize,
    ) -> impl Future<Output = Result<Vec<RankedSkill>, Self::Error>> + Send;
}

/// Multi-agent CRDT merge across namespaces (06). Implemented in M4.
pub trait Merge: Send + Sync {
    /// The merge request: the two namespaced states to reconcile.
    type Request: Send;
    /// The merge resolution: the reconciled state plus conflict records.
    type Resolution: Send;
    /// The typed error this seam surfaces.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Merge two namespaced states deterministically (06).
    fn merge(
        &self,
        request: Self::Request,
    ) -> impl Future<Output = Result<Self::Resolution, Self::Error>> + Send;
}

/// Decay, active forgetting, and the hard-erasure cascade (05). Implemented in M5.
pub trait Forgetting: Send + Sync {
    /// A summary of a hard-erasure cascade (e.g. the count of cascaded nodes/edges).
    type EraseReport: Send;
    /// The typed error this seam surfaces.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Soft-forget a memory: set `expired_at`; reversible; audited `forget` (05).
    fn forget(&self, id: Id) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Hard-erase a memory and its derivation cascade: irreversible; audited `purge` (05).
    fn erase(&self, id: Id) -> impl Future<Output = Result<Self::EraseReport, Self::Error>> + Send;
}

/// The OpenAI-compatible embedding client (08 §1). Implemented in M0.T08.
///
/// The one contract expressible entirely in domain terms today: it consumes text
/// and produces validated [`Embedding`]s, recording the [`EmbedderModel`] identity
/// for the startup dimension-consistency check and the cross-family guard.
pub trait Embedder: Send + Sync {
    /// The typed error this seam surfaces.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Embed a batch in input order; a wrong returned vector count is an error (08 §1).
    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send;

    /// The identity of the model this embedder produces vectors with.
    fn model(&self) -> &EmbedderModel;
}

/// A shared embedder is itself an embedder, so one client can back several
/// subsystems (the capture path and the retrieval path share one) without being
/// cloneable — embedders hold secret material that must not be copied around.
impl<E: Embedder + ?Sized> Embedder for std::sync::Arc<E> {
    type Error = E::Error;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        (**self).embed(inputs)
    }

    fn model(&self) -> &EmbedderModel {
        (**self).model()
    }
}

/// The outcome of the capture-path privacy/injection filter (04 §1, 02 §6.1, 07).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterOutcome {
    /// The content after sensitive spans were redacted.
    pub cleaned: String,
    /// The redactions applied, recorded in `Episode.origin` (02 §6.1).
    pub redactions: Vec<Redaction>,
    /// Ids of detected prompt-injection markers, recorded in `Episode.origin`.
    pub injection_flags: Vec<String>,
    /// Per-marker applied-hit counts for tuning (M6.T03): `(marker_id, count)` in marker
    /// declaration order, one entry per marker that fired at least once. Unlike
    /// [`injection_flags`](Self::injection_flags) (which de-duplicates to one id per marker),
    /// this records how many times each marker was applied in this content, so corpus
    /// measurement can attribute block-rate to individual patterns. (A marker whose match is
    /// fully covered by an earlier-declared overlapping marker is dropped by the fail-closed
    /// edit walk and not counted — the count is applied edits, not raw regex firings.)
    ///
    /// This is observability metadata only: it is deliberately **not** folded into the
    /// content hash or `Episode.origin` (which record `cleaned`/`redactions`/`injection_flags`),
    /// so adding or tuning a marker never perturbs the canonical stored bytes of an episode.
    pub marker_hits: Vec<(String, u32)>,
}

/// The residue-only substance floor: a marker-flagged capture whose cleaned content keeps
/// fewer alphanumeric characters than this (and lost most of its substance to excision) is
/// refused. Calibrated on the 2026-06-11 live probe (14 remaining of 64) with headroom for
/// short real notes — a flagged capture keeping 24+ word characters always stores.
pub const RESIDUE_SUBSTANCE_FLOOR: usize = 24;

impl FilterOutcome {
    /// True when injection-marker excision removed the substance of the content, leaving a
    /// fragment not worth remembering (07 §5 rider, found in the 2026-06-11 test drive: an
    /// excised probe left the junk episode "and immediately." that surfaced in recall).
    ///
    /// The capture funnel refuses a residue-only write fail-closed, with a
    /// `residue_rejected` audit. Two invariants bound the policy:
    ///
    /// - **Never fires on unflagged content** (`injection_flags` empty ⇒ `false`): a short
    ///   benign capture ("ok.", "done") is not residue, so the benign false-positive rate
    ///   stays exactly where M6.T03 pinned it — markers are the only trigger.
    /// - **Never fires when substance survives**: a long legitimate message that quoted one
    ///   injection phrase keeps its episode; only a capture *hollowed out* by excision is
    ///   refused.
    ///
    /// `original` is the pre-filter content, available so the policy can weigh how much of
    /// the write the excision consumed rather than judging the residue's length alone.
    /// The policy is a hybrid: the residue must be small in absolute substance (fewer
    /// than [`RESIDUE_SUBSTANCE_FLOOR`] remaining alphanumeric characters — the live probe
    /// left 14) AND the excision must have consumed most of the write (over half its
    /// alphanumeric substance). The floor keeps long quotes safe on its own; the ratio
    /// keeps a short-but-mostly-intact capture safe when a marker only grazed it.
    /// Redaction placeholders like `[redacted:email]` count toward substance, which is
    /// correct: a privacy-redacted capture is a real memory (and arrives here unflagged
    /// anyway).
    #[must_use]
    pub fn is_residue_only(&self, original: &str) -> bool {
        if self.injection_flags.is_empty() {
            return false;
        }
        let substance = |text: &str| text.chars().filter(|c| c.is_alphanumeric()).count();
        let remaining = substance(&self.cleaned);
        remaining < RESIDUE_SUBSTANCE_FLOOR && remaining * 2 < substance(original)
    }
}

/// The privacy and prompt-injection filter on the capture hot path (04 §1, 07).
/// Implemented in M1.T02; hardened against a published injection corpus in M6.T03.
///
/// Synchronous because v1.0.0 filtering is local (configured redaction patterns
/// plus known-marker detection), so it adds no network round-trip to capture.
pub trait PrivacyFilter: Send + Sync {
    /// The typed error this seam surfaces.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Redact sensitive spans and flag injection markers in raw capture content (04 §1).
    fn filter(&self, content: &str) -> Result<FilterOutcome, Self::Error>;
}

/// A subject or object surface form as an extractor read it, before entity
/// resolution maps it to a canonical [`Entity`](crate::nodes::semantic::Entity).
///
/// Carried for both the fact's subject and an entity-typed object so the two share
/// one shape; the `entity_type` is the extractor's provisional guess that
/// resolution may refine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntitySurface {
    /// The surface string exactly as it appeared in the episode.
    pub surface: String,
    /// The extractor's provisional entity type (e.g. `Person`, `Project`, `Tool`).
    pub entity_type: String,
}

/// A fact's object as the extractor produced it (04 §2).
///
/// Either another entity (still a surface form awaiting resolution to an
/// `Entity.id`) or a literal already in its final typed [`ObjectValue`] form.
#[derive(Debug, Clone, PartialEq)]
pub enum ExtractedObject {
    /// An entity reference; the surface resolves to a canonical `Entity.id`.
    Entity(EntitySurface),
    /// A settled literal value.
    Literal(ObjectValue),
}

/// One candidate fact an extractor drew from an episode, before materialization
/// (04 §2).
///
/// The subject and an entity-typed object are SURFACE forms — the resolution
/// pipeline maps them to canonical entity ids before the fact is written.
/// `confidence` and `source_spans` flow into `Fact.confidence` and
/// `Fact.extraction` so every stored assertion carries its provenance (02 §6.2).
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractedFact {
    /// The subject surface form and its provisional type.
    pub subject: EntitySurface,
    /// The relation.
    pub predicate: String,
    /// The object — an entity surface form or a settled literal.
    pub object: ExtractedObject,
    /// Extraction confidence in `[0, 1]`.
    pub confidence: f64,
    /// Canonical natural-language rendering of the assertion (the BM25/embedding
    /// surface once written to `Fact.statement`).
    pub statement: String,
    /// The episode byte spans the assertion was drawn from.
    pub source_spans: Vec<SourceSpan>,
}

/// The identity of the extractor that produced a batch of facts (02 §6.2).
///
/// Recorded into every fact's [`Extraction`](crate::nodes::semantic::Extraction)
/// provenance so the M6 cross-family consolidation guard can later refuse to mix
/// assertions drawn by incompatible model families.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractorIdentity {
    /// Extractor model family, if model-backed (`None` for a pure rule extractor).
    pub model_family: Option<String>,
    /// Extractor model version, if model-backed.
    pub model_version: Option<String>,
    /// Version of the extraction rule set that produced the facts.
    pub rule_version: String,
}

/// The fact-extraction seam (04 §2): turn one episode into candidate facts.
///
/// Mirrors [`Embedder`]'s `-> impl Future + Send` shape rather than `async fn` so
/// the returned future's `Send` bound stays explicit for the multi-threaded
/// runtime. The production implementation is model-backed (deferred to M4); M2
/// ships a deterministic rule-based extractor so the consolidation tests stay
/// hermetic and idempotency rests on a reproducible key.
pub trait FactExtractor: Send + Sync {
    /// The typed error this seam surfaces.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Extract candidate facts from one episode's content.
    fn extract(
        &self,
        episode: &Episode,
    ) -> impl Future<Output = Result<Vec<ExtractedFact>, Self::Error>> + Send;

    /// The identity recorded into every produced fact's extraction provenance.
    fn identity(&self) -> &ExtractorIdentity;
}

/// A shared extractor is itself an extractor, so one instance can back both the
/// consolidation pass and any future inline-extraction caller without being
/// cloneable — a model-backed extractor may hold secret material that must not be
/// copied around (mirrors the [`Embedder`] `Arc` forwarding).
impl<X: FactExtractor + ?Sized> FactExtractor for std::sync::Arc<X> {
    type Error = X::Error;

    fn extract(
        &self,
        episode: &Episode,
    ) -> impl Future<Output = Result<Vec<ExtractedFact>, Self::Error>> + Send {
        (**self).extract(episode)
    }

    fn identity(&self) -> &ExtractorIdentity {
        (**self).identity()
    }
}

/// The identity of the summarizer that produced a note (04 §2, M2.T06).
///
/// Mirrors [`ExtractorIdentity`]: recorded so a later cross-family guard can tell which
/// model family (or rule set) condensed a cluster, and so re-running the same rule version
/// reproduces the same content-addressed note id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummarizerIdentity {
    /// Summarizer model family, if model-backed (`None` for a pure rule summarizer).
    pub model_family: Option<String>,
    /// Summarizer model version, if model-backed.
    pub model_version: Option<String>,
    /// Version of the summarization rule set that produced the note.
    pub rule_version: String,
}

/// A cluster of facts about one subject, handed to the [`Summarizer`] to condense (M2.T06).
///
/// The consolidation pass builds the cluster (which subject, which facts, over what window);
/// the summarizer only turns it into prose. The note's content-addressed id is derived by
/// the pass from the source fact set, not here, so the summarizer stays free of id policy.
#[derive(Debug, Clone)]
pub struct SummarizationCluster {
    /// The subject entity every fact in the cluster is about.
    pub subject_id: Id,
    /// The subject's canonical name, for readable prose and keywords.
    pub subject_name: String,
    /// The facts to summarize (all share `subject_id`).
    pub facts: Vec<Fact>,
    /// Distinct entity names referenced across the facts (the subject plus entity-typed
    /// objects), the surface the detail-retention guard checks the summary preserves.
    pub entity_names: Vec<String>,
}

/// What a [`Summarizer`] produced for one cluster: a note body and its recall surface.
///
/// The pass attaches lineage (the source facts) and the content-addressed id; the
/// summarizer returns only the prose, keywords, and optional context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryOutput {
    /// The note body.
    pub content: String,
    /// Keywords for lexical recall and the detail-retention surface.
    pub keywords: Vec<String>,
    /// Optional surrounding context that situates the summary.
    pub context: Option<String>,
}

/// The summarization seam (04 §2): condense a cluster of facts into a higher-level note.
///
/// Conservative by contract: `summarize` returns `None` to skip a cluster it cannot
/// condense safely (too small, too thin), so a thin cluster yields no lossy artifact. The
/// production implementation is model-backed (deferred to M4); M2 ships a deterministic
/// rule summarizer so consolidation tests stay hermetic and the note id stays reproducible.
pub trait Summarizer: Send + Sync {
    /// The typed error this seam surfaces.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Condense a cluster into a candidate note, or `None` to conservatively skip it.
    fn summarize(
        &self,
        cluster: &SummarizationCluster,
    ) -> impl Future<Output = Result<Option<SummaryOutput>, Self::Error>> + Send;

    /// The identity recorded into every produced note's provenance.
    fn identity(&self) -> &SummarizerIdentity;
}

/// A shared summarizer is itself a summarizer (mirrors the [`FactExtractor`] forwarding),
/// so one instance can back the consolidation pass without being cloned.
impl<S: Summarizer + ?Sized> Summarizer for std::sync::Arc<S> {
    type Error = S::Error;

    fn summarize(
        &self,
        cluster: &SummarizationCluster,
    ) -> impl Future<Output = Result<Option<SummaryOutput>, Self::Error>> + Send {
        (**self).summarize(cluster)
    }

    fn identity(&self) -> &SummarizerIdentity {
        (**self).identity()
    }
}

/// The identity of the inducer that auto-derived a skill during consolidation (05 §1, M3.T06).
///
/// Mirrors [`SummarizerIdentity`]: recorded so a later cross-family guard can tell which model
/// family (or rule set) induced a skill, and so re-running the same rule version reproduces the
/// same content-addressed skill id. The `rule_version` also keys the induced skill's id, so a
/// bump cuts a fresh induced version under the same name rather than colliding with the old one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InducerIdentity {
    /// Inducer model family, if model-backed (`None` for a pure rule inducer).
    pub model_family: Option<String>,
    /// Inducer model version, if model-backed.
    pub model_version: Option<String>,
    /// Version of the induction rule set that produced the skill.
    pub rule_version: String,
}

/// The reuse evidence the consolidation pass gathered for one episode, handed to the
/// [`SkillInducer`] (05 §1, M3.T06).
///
/// The pass owns the evidence policy — how many times the procedure recurred, over what window,
/// in which namespace — and computes the count from the committed graph; the inducer only turns
/// the episode into an inert candidate procedure. Keeping the count out here lets a model-backed
/// inducer condition on the strength of the evidence without re-querying the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InductionContext {
    /// How many live episodes in the namespace carried this exact content (the reuse evidence).
    pub recurrence_count: usize,
}

/// What a [`SkillInducer`] rendered from a recurring episode: an inert candidate procedure
/// (05 §1, M3.T06).
///
/// Only the procedure itself — the id, name, version, namespace, and `induced` flag are the
/// pass's policy (kept out of the inducer exactly as the note id is kept out of the
/// [`Summarizer`]), so the inducer cannot mint an id, escape its namespace, or clear the
/// `induced` flag. The body is stored as data and is **never executed** by the substrate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InducedSkill {
    /// A human-readable description; the BM25 recall surface for the induced skill.
    pub description: String,
    /// Language tag of the body.
    pub language: String,
    /// The procedure itself, stored as data (never executed).
    pub body: String,
}

/// The skill-induction seam (05 §1): conservatively derive a reusable procedure from a
/// recurring episode, off by default.
///
/// Conservative by contract: `induce` returns `None` to decline an episode it will not turn
/// into a skill, so a thin or unsuitable episode yields no artifact — and the pass gates the
/// call on reuse evidence (repetition), a procedural role, and the private namespace before the
/// inducer is ever consulted. The production implementation would be model-backed (deferred to
/// the optional M3.S3 distillation layer); M3 ships a deterministic rule inducer (its
/// [`Error`](SkillInducer::Error) is [`Infallible`](std::convert::Infallible)) so consolidation
/// stays hermetic and the induced skill id stays reproducible. An induced skill is confined to
/// the private namespace and never auto-promoted across a trust boundary.
pub trait SkillInducer: Send + Sync {
    /// The typed error this seam surfaces.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Render a recurring episode into a candidate procedure, or `None` to decline it.
    fn induce(
        &self,
        episode: &Episode,
        cx: &InductionContext,
    ) -> impl Future<Output = Result<Option<InducedSkill>, Self::Error>> + Send;

    /// The identity recorded into every induced skill's provenance.
    fn identity(&self) -> &InducerIdentity;
}

/// A shared inducer is itself an inducer (mirrors the [`Summarizer`] forwarding), so one
/// instance can back the consolidation pass without being cloned.
impl<I: SkillInducer + ?Sized> SkillInducer for std::sync::Arc<I> {
    type Error = I::Error;

    fn induce(
        &self,
        episode: &Episode,
        cx: &InductionContext,
    ) -> impl Future<Output = Result<Option<InducedSkill>, Self::Error>> + Send {
        (**self).induce(episode, cx)
    }

    fn identity(&self) -> &InducerIdentity {
        (**self).identity()
    }
}

/// The identity of the link-evolver that drew or revised a note link (M3.T09).
///
/// Mirrors [`SummarizerIdentity`]: recorded in every `link_evolve` audit so a later cross-family
/// guard can tell which model family (or rule set) proposed a relationship, and so the
/// deterministic rule evolver attributes its calls to a stable actor across runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkEvolverIdentity {
    /// Evolver model family, if model-backed (`None` for a pure rule evolver).
    pub model_family: Option<String>,
    /// Evolver model version, if model-backed.
    pub model_version: Option<String>,
    /// Version of the link-evolution rule set that proposed the link.
    pub rule_version: String,
}

/// One relationship a [`LinkEvolver`] proposes from the source note to a candidate (M3.T09).
///
/// The `target_id` must be one of the candidate notes the evolver was offered — the driver drops
/// a proposal that points anywhere else. `relationship_label` must be in the closed vocabulary
/// (never free text, an anti-injection / anti-drift constraint); the driver re-validates it.
/// `confidence` is the evolver's strength in `[0, 1]`; the driver drops proposals below its floor.
#[derive(Debug, Clone, PartialEq)]
pub struct EvolvedLink {
    /// The candidate note this link points to (must be one of the offered candidates).
    pub target_id: Id,
    /// The relationship label, from the closed vocabulary (the driver re-validates).
    pub relationship_label: String,
    /// Confidence in `[0, 1]`; below the driver's floor the proposal is dropped.
    pub confidence: f64,
}

/// The note-link-evolution seam (M3.T09): propose relationships from one source note to a bounded
/// set of nearby candidate notes, off by default and off the consolidation cursor.
///
/// Conservative by contract: `evolve` returns `None` to decline a source note it cannot evaluate
/// (the model is unavailable, the reply is unusable), so the driver records the call and writes no
/// edge — degrading to the deterministic rule tier rather than failing. An `Ok(Some(_))` with an
/// empty vector means the evolver ran and found no relationship worth drawing. The driver owns all
/// id, bi-temporal, and cascade-guard policy (which candidates to offer, the confidence floor, the
/// per-run caps, create-vs-revise); the evolver only judges relatedness, exactly as the
/// [`Summarizer`] only condenses and the note id stays the pass's policy.
pub trait LinkEvolver: Send + Sync {
    /// The typed error this seam surfaces.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Propose relationships from `source` to any of `candidates`, or `None` to decline.
    fn evolve(
        &self,
        source: &Note,
        candidates: &[Note],
    ) -> impl Future<Output = Result<Option<Vec<EvolvedLink>>, Self::Error>> + Send;

    /// The identity recorded into every proposed link's provenance.
    fn identity(&self) -> &LinkEvolverIdentity;
}

/// A shared evolver is itself an evolver (mirrors the [`Summarizer`] forwarding), so one instance
/// can back the off-cursor driver without being cloned.
impl<L: LinkEvolver + ?Sized> LinkEvolver for std::sync::Arc<L> {
    type Error = L::Error;

    fn evolve(
        &self,
        source: &Note,
        candidates: &[Note],
    ) -> impl Future<Output = Result<Option<Vec<EvolvedLink>>, Self::Error>> + Send {
        (**self).evolve(source, candidates)
    }

    fn identity(&self) -> &LinkEvolverIdentity {
        (**self).identity()
    }
}
