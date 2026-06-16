//! The hybrid retriever: router → signals → fusion → recall bundle (03).
//!
//! [`HybridRetriever`] implements the domain [`Retriever`] contract. It classifies the
//! query, runs the weighted signals the mode profile calls for, fuses them, authorizes
//! and diversity-caps the candidate set, and assembles the [`RecallBundle`]. It is
//! generic over the [`Embedder`] seam; when the embedder is unreachable the dense
//! signal drops out and retrieval degrades to the rest, flagged in the explanation
//! (03 §6, §8.1).
//!
//! The retriever composes eight signal types: lexical, lexical-anchor, dense,
//! support-expansion, and associative-graph search the graph (the support and graph
//! signals gated to the classes the router enables expansion for, 03 §3); trust,
//! importance, and recency re-rank the surfaced set, the latter two only when the
//! caller supplies a clock (05 §2).

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aionforge_domain::authz::{Authorizer, DefaultAuthorizer};
use aionforge_domain::contracts::{Embedder, Retriever};
use aionforge_domain::embedding::Embedding;
use aionforge_store::{CandidateSet, NodeId, SearchKind, Store};
use tracing::Instrument;

use crate::bundle::{RecallBundle, RecallExplanation, StageTimings, render};
use crate::error::RetrievalError;
use crate::fusion::{DEFAULT_RRF_K, WeightedRanking, fuse};
use crate::precision::{derive_graph_seed, resolve_seed_entities};
use crate::query::{RecallQuery, TemporalMode};
use crate::rerank;
use crate::router::{looks_like_source_anchor, profile_for, route};
use crate::selection::{
    bail_if_past, core_block_entries, effective_fanout, fact_dense_ranking, fact_graph_ranking,
    fact_lexical_ranking, fact_support_ranking, lexical_anchor_ranking, select, trust_rankings,
};
use crate::signals::{
    Signal, dense_ranking_in_nodes, embed_query, graph_ranking_for, lexical_ranking_in_nodes,
};
use crate::trace;

/// The hard ceiling on [`RetrieverConfig::support_expansion_depth`] — the "bounded" half
/// of the M3.T02 depth/fan-out knob. v1 expands a single `SUPPORTS` hop; deeper transitive
/// expansion is a future extension, and the knob already carries the requested depth.
const MAX_EXPANSION_DEPTH: usize = 1;

/// The number of lexical ranks that receive the factual-query anchor. Keeping the
/// window small protects precise surface matches without making every BM25 hit count
/// twice.
const LEXICAL_ANCHOR_WINDOW: usize = 3;

/// Tuning for the retriever that is not per-query.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetrieverConfig {
    /// How many candidates each signal pulls before fusion, when a query does not set
    /// its own fan-out. A wider fan-out gives fusion and the diversity cap more to
    /// work with at the cost of more candidate reads.
    pub default_fanout: usize,
    /// How many `SUPPORTS` hops the additive support signal expands the query-entity roots
    /// through to recover their supporting evidence (03 §1, §4, M3.T02). `0` disables
    /// support expansion (the dense pass alone stands); the value is clamped to
    /// `MAX_EXPANSION_DEPTH`. v1 expands a single hop.
    pub support_expansion_depth: usize,
    /// Whether elapsed time sinks effective importance in the importance re-rank (05 §2,
    /// M5.T01). Off by default: the re-rank then orders by the raw stored importance —
    /// the signal still participates; this switch governs only whether time decays the
    /// value. The engine maps `DecayConfig` here.
    pub decay_enabled: bool,
    /// Half-life for episodic memory, in seconds, when decay is enabled.
    pub episodic_half_life_secs: f64,
    /// Half-life for semantic and identity memory, in seconds, when decay is enabled.
    pub semantic_half_life_secs: f64,
    /// Whether a fact's cooling stamp reduces its rank-time trust (05 §1, M5.T05).
    /// Off by default; the host maps `DriftConfig.enabled` here. Like decay, the
    /// modulation also needs the caller-stamped `options.now` — without a clock,
    /// recall is byte-identical to a pre-cooling one.
    pub cooling_enabled: bool,
    /// The multiplier applied to a cooled fact's trust while `now` sits inside its
    /// window, when cooling is enabled. In `(0, 1]`; out-of-range values are inert
    /// (the domain guard never zeroes a rank on misconfiguration).
    pub cooling_factor: f64,
    /// The wall-clock budget applied to a recall whose caller left `RecallOptions::deadline`
    /// unset (03 §8). `None` leaves an un-budgeted recall unbounded. This is the live cost
    /// guard for the namespace-scoped episode scans: the scoped lexical/dense passes sweep
    /// the reader's whole visible scope, and the engine's per-block cancellation only fires
    /// when some deadline exists — so a deployment default here is what bounds a recall over a
    /// large team namespace. An explicit per-query deadline always takes precedence (03 §6).
    pub default_recall_budget: Option<Duration>,
}

impl Default for RetrieverConfig {
    fn default() -> Self {
        Self {
            default_fanout: 50,
            support_expansion_depth: 1,
            decay_enabled: false,
            episodic_half_life_secs: 604_800.0,
            semantic_half_life_secs: 31_536_000.0,
            cooling_enabled: false,
            cooling_factor: 0.5,
            // A generous safety ceiling, not a tuning target: a healthy recall finishes in
            // milliseconds, so 5s never trips a normal query but bounds the scoped scans on a
            // busy store. The host overrides this from `[retrieval] recall_deadline_ms`.
            default_recall_budget: Some(Duration::from_secs(5)),
        }
    }
}

/// A hybrid retriever over a shared store and an embedder.
pub struct HybridRetriever<E> {
    store: Arc<Store>,
    embedder: E,
    config: RetrieverConfig,
    authorizer: Arc<dyn Authorizer>,
}

impl<E: Embedder> HybridRetriever<E> {
    /// Build a retriever over a shared store, an embedder, and its config, with the
    /// default namespace read policy ([`DefaultAuthorizer`]).
    #[must_use]
    pub fn new(store: Arc<Store>, embedder: E, config: RetrieverConfig) -> Self {
        Self::with_authorizer(store, embedder, config, Arc::new(DefaultAuthorizer))
    }

    /// Build a retriever with an explicit [`Authorizer`], so a host's read policy governs
    /// what a recall may surface. The engine injects the same authority it checks writes
    /// against, so reads and writes share one namespace boundary (06 §1).
    #[must_use]
    pub fn with_authorizer(
        store: Arc<Store>,
        embedder: E,
        config: RetrieverConfig,
        authorizer: Arc<dyn Authorizer>,
    ) -> Self {
        Self {
            store,
            embedder,
            config,
            authorizer,
        }
    }

    /// Run one recall.
    async fn run(&self, query: RecallQuery) -> Result<RecallBundle, RetrievalError> {
        let span = trace::recall_span(&query);
        let result = self.run_inner(query).instrument(span.clone()).await;
        trace::record_recall_result(&span, &result);
        result
    }

    async fn run_inner(&self, query: RecallQuery) -> Result<RecallBundle, RetrievalError> {
        let started = Instant::now();
        // A per-query deadline wins; otherwise the deployment's default recall budget applies,
        // so the scoped episode scans (and the engine's per-block cancellation) always run
        // against some ceiling rather than open-ended on a busy store (03 §6, §8).
        let deadline = query
            .options
            .deadline
            .or(self.config.default_recall_budget)
            .map(|budget| started + budget);

        // 1. Classify (or honor an override).
        let profile = trace::stage_span("classify").in_scope(|| {
            query
                .options
                .mode_override
                .map_or_else(|| route(&query.text), profile_for)
        });
        let classify_ms = started.elapsed().as_millis();
        bail_if_past(deadline)?;

        // The reader's visible namespace set, computed UP FRONT so episode candidate
        // generation can be scoped to it (06 §1, 03 §6). Unlike facts (which the current-
        // support set already bounds), the episode lexical/dense scans would otherwise sweep
        // every namespace and spend the per-signal fan-out on episodes the reader cannot see,
        // then drop them in the post-fusion authorization filter — starving a reader whose
        // namespace is a minority of the store. The admin reveal (07 §4) widens the set in
        // lockstep with `include_system`; the assemble stage reuses this same `visible`.
        let surface_system =
            query.options.include_system && self.authorizer.may_surface_system(&query.principal);
        let visible = {
            let base = self.authorizer.visible_namespaces(&query.principal);
            if surface_system {
                base.with_system()
            } else {
                base
            }
        };
        // Every episode in a visible namespace, by id — the scope the episode lexical, dense,
        // and graph signals generate candidates over. The per-signal fan-out then caps how many
        // of these each signal ranks, spent entirely on episodes the reader may see. The graph
        // episode signal scopes through `algo.pagerank`'s `result_nodes` (intersect before
        // truncate), so it no longer leans on the post-fusion filter in `select` to bound it.
        let episode_scope = self
            .store
            .episode_nodes_in_namespaces(&visible.namespaces())?;
        // Enumerating the visible scope is itself an indexed scan over the namespace; on a
        // busy store it can be the first place a tight recall budget is spent, so bail here
        // before fanning the signals over it rather than only at the next stage boundary.
        bail_if_past(deadline)?;

        // 2. Run the signals the profile weights call for, over both episodes and facts.
        //    Lexical and dense are the signals implemented this milestone. The query is
        //    embedded once and the same vector is fanned across the kinds, so a recall
        //    never embeds twice; the temporal mode shapes which facts a fact search may
        //    see (03 §1, §5). Fact node ids are remembered so `select` knows which fused
        //    candidates to resolve and temporally filter as facts.
        let signals_started = Instant::now();
        let fanout = effective_fanout(&query, &self.config);
        let mut rankings: Vec<WeightedRanking> = Vec::new();
        let mut signals_run: Vec<Signal> = Vec::new();
        let mut fact_nodes: HashSet<NodeId> = HashSet::new();
        let mut embedder_available = true;

        // Embed the query a single time, if any dense weight asks for it. A `None`
        // embedding is the embedder-down signal: every dense ranking is then skipped and
        // retrieval degrades to lexical (03 §6, §8.1).
        let query_embedding: Option<Embedding> = if profile.weights.dense > 0.0 {
            let embedding = embed_query(&self.embedder, &query.text)
                .instrument(trace::query_embed_span(fanout))
                .await;
            embedder_available = embedding.is_some();
            embedding
        } else {
            None
        };

        // The current-support set a sensitive query reads against is the provenance-grounded
        // one (03 §4): a single choice that scopes every Current-mode fact signal — lexical,
        // the composed high-precision dense, and its fallback — so an ungrounded fact never
        // leaks in through a path that forgot the flag.
        let support_set = if query.options.sensitive {
            CandidateSet::ProvenanceCurrentSupportFacts
        } else {
            CandidateSet::CurrentSupportFacts
        };

        // In Current mode the fact searches are scoped to the live support set. Resolve its
        // membership once: an empty set short-circuits both fact searches, and the non-empty
        // list scopes the lexical BM25 search (there is no `text_score_candidate_state`
        // primitive, so it passes the explicit node list). `None` in any other temporal mode
        // means "search all facts" (03 §5).
        let current_facts: Option<Vec<NodeId>> = match query.options.temporal {
            TemporalMode::Current => Some(self.store.candidate_state_members(support_set)?),
            _ => None,
        };

        if profile.weights.lexical > 0.0 {
            let _signal_span = trace::signal_span(Signal::Lexical, fanout).entered();
            // Episodes are scoped to the reader's visible namespaces (06 §1, 03 §6); facts
            // ride their own current-support scoping below.
            let episodes = lexical_ranking_in_nodes(
                &self.store,
                SearchKind::Episode,
                &query.text,
                &episode_scope,
                fanout,
                deadline,
            )?;
            let facts = fact_lexical_ranking(
                &self.store,
                &query,
                current_facts.as_deref(),
                fanout,
                deadline,
            )?;
            let lexical_anchor = if profile.weights.lexical_anchor > 0.0 {
                let _anchor_span =
                    trace::signal_span(Signal::LexicalAnchor, LEXICAL_ANCHOR_WINDOW).entered();
                if looks_like_source_anchor(&query.text) {
                    lexical_anchor_ranking(&[&episodes], LEXICAL_ANCHOR_WINDOW)
                } else {
                    lexical_anchor_ranking(&[&episodes, &facts], LEXICAL_ANCHOR_WINDOW)
                }
            } else {
                None
            };
            fact_nodes.extend(facts.candidates.iter().map(|c| c.node));
            rankings.push(WeightedRanking::new(profile.weights.lexical, episodes));
            rankings.push(WeightedRanking::new(profile.weights.lexical, facts));
            signals_run.push(Signal::Lexical);
            if let Some(anchor) = lexical_anchor {
                rankings.push(WeightedRanking::new(profile.weights.lexical_anchor, anchor));
                signals_run.push(Signal::LexicalAnchor);
            }
        }
        bail_if_past(deadline)?;

        if let Some(embedding) = &query_embedding {
            let _signal_span = trace::signal_span(Signal::Dense, fanout).entered();
            // Episodes are scoped to the reader's visible namespaces and exact-scored over
            // that set (06 §1, 03 §6); the fact dense path keeps its current-support scoping
            // and high-precision graph-seed composition below.
            let episodes = dense_ranking_in_nodes(
                &self.store,
                SearchKind::Episode,
                embedding,
                &episode_scope,
                fanout,
                deadline,
            )?;
            // The high-precision default path (03 §4): for the factual/temporal-current
            // classes, derive a graph candidate seed (scope membership, else the entities
            // the query names) and compose it with the current-support set. Other classes
            // and the historical temporal modes leave the seed `None` and use the plain
            // current/global fact dense path.
            let graph_seed = if profile.exact_rerank
                && matches!(query.options.temporal, TemporalMode::Current)
            {
                derive_graph_seed(&self.store, Some(embedding))?
            } else {
                None
            };
            let facts = fact_dense_ranking(
                &self.store,
                current_facts.as_deref(),
                graph_seed.as_deref(),
                support_set,
                embedding,
                fanout,
                profile.exact_rerank,
                deadline,
            )?;
            fact_nodes.extend(facts.candidates.iter().map(|c| c.node));
            rankings.push(WeightedRanking::new(profile.weights.dense, episodes));
            rankings.push(WeightedRanking::new(profile.weights.dense, facts));
            signals_run.push(Signal::Dense);
        }
        bail_if_past(deadline)?;

        // The support-expansion signal (03 §1, §4, M3.T02) — additive to dense, never a
        // replacement. For the graph-expansion classes in Current mode, expand the
        // query-entity fact roots one incoming `SUPPORTS` hop and vector-score the
        // roots-plus-evidence set, composed with the current-support set. This recovers a
        // relevant fact's far-embedded supporting evidence that the global dense ANN ranks
        // out of its top-k, while the dense pass above keeps scoring every current fact —
        // so a near, non-root current fact keeps its full dense contribution and current
        // precision stays whole (the §4 precision floor). Gated to a non-zero, capped depth
        // knob; the roots are the same query-entity facts the high-precision seed derives,
        // and the gating classes are disjoint from the seed's, so a recall resolves them at
        // most once. No resolvable entity (empty roots) skips the signal rather than
        // running an unscoped expansion.
        if profile.graph_expansion
            && profile.weights.support > 0.0
            && matches!(query.options.temporal, TemporalMode::Current)
            && self.config.support_expansion_depth.min(MAX_EXPANSION_DEPTH) >= 1
            && let Some(embedding) = &query_embedding
            && let Some(roots) = derive_graph_seed(&self.store, Some(embedding))?
            && !roots.is_empty()
        {
            let _signal_span = trace::signal_span(Signal::Support, fanout).entered();
            let facts = fact_support_ranking(&self.store, &roots, support_set, embedding, fanout)?;
            fact_nodes.extend(facts.candidates.iter().map(|c| c.node));
            rankings.push(WeightedRanking::new(profile.weights.support, facts));
            signals_run.push(Signal::Support);
        }
        bail_if_past(deadline)?;

        // The associative graph signal (03 §1, §3): for the classes the router turns graph
        // expansion on for (multi-hop, entity), seed Personalized PageRank on the entities
        // the query names and spread mass to the facts and episodes around them. Seeds
        // resolve from the entity text index (always) and the query vector (when embedded),
        // so a named-entity query still expands when the embedder is down. Like dense it
        // runs per kind, so `select` routes each fused candidate by `fact_nodes` membership;
        // the fact side is current-scoped here while the episode side rides the standard
        // episode admission in `select`. No resolvable entity means no seed and the signal
        // is simply skipped — never an unseeded (global) PageRank.
        if profile.graph_expansion
            && profile.weights.graph > 0.0
            && let Some(seeds) =
                resolve_seed_entities(&self.store, &query.text, query_embedding.as_ref())?
        {
            let _signal_span = trace::signal_span(Signal::Graph, fanout).entered();
            // Scope the episode graph ranking to the reader's visible namespaces via
            // `result_nodes`, the same `episode_scope` the lexical/dense episode signals use,
            // so the graph fan-out is spent on in-scope episodes (03 §6).
            let episodes = graph_ranking_for(
                &self.store,
                SearchKind::Episode,
                &seeds,
                fanout,
                Some(&episode_scope),
                deadline,
            )?;
            let facts = fact_graph_ranking(
                &self.store,
                &seeds,
                current_facts.as_deref(),
                fanout,
                deadline,
            )?;
            fact_nodes.extend(facts.candidates.iter().map(|c| c.node));
            rankings.push(WeightedRanking::new(profile.weights.graph, episodes));
            rankings.push(WeightedRanking::new(profile.weights.graph, facts));
            signals_run.push(Signal::Graph);
        }
        bail_if_past(deadline)?;

        // The trust re-rank (06 §5): order the candidates the search signals already surfaced by
        // their stored trust — `Fact.stats.trust` (the reliability-folded value, M4.T05) for facts,
        // `Episode.stats.trust` for episodes — so a low-trust fact sinks and a high-trust one rises
        // within the relevant set. Trust is a quality re-rank, never a retrieval: it ranks only what
        // the other signals found, per kind, so RRF folds it in like any other ranking and `select`
        // routes each candidate by `fact_nodes` membership exactly as before. Skipped when the class
        // gives trust no weight or no signal produced a candidate.
        if profile.weights.trust > 0.0 {
            let _signal_span = trace::signal_span(Signal::Trust, fanout).entered();
            let (fact_trust, episode_trust) = trust_rankings(
                &self.store,
                &self.config,
                &rankings,
                &fact_nodes,
                query.options.now.as_ref(),
            )?;
            let ran = !fact_trust.candidates.is_empty() || !episode_trust.candidates.is_empty();
            if !fact_trust.candidates.is_empty() {
                rankings.push(WeightedRanking::new(profile.weights.trust, fact_trust));
            }
            if !episode_trust.candidates.is_empty() {
                rankings.push(WeightedRanking::new(profile.weights.trust, episode_trust));
            }
            if ran {
                signals_run.push(Signal::Trust);
            }
        }

        // The importance and recency re-ranks (05 §2, M5.T01): order the same surfaced set
        // by effective (decayed) importance and by ingestion recency, each folded into RRF
        // exactly like trust. Both exist only when the caller stamped `options.now` — there
        // is no ambient clock in the retrieval path, so a query without a clock recalls
        // byte-identically to a pre-decay one.
        if let Some(now) = &query.options.now {
            let (fact_set, episode_set) = rerank::surfaced(&rankings, &fact_nodes);
            if profile.weights.importance > 0.0 {
                let _signal_span = trace::signal_span(Signal::Importance, fanout).entered();
                let facts =
                    rerank::importance_ranking(&self.store, &fact_set, true, now, &self.config)?;
                let episodes = rerank::importance_ranking(
                    &self.store,
                    &episode_set,
                    false,
                    now,
                    &self.config,
                )?;
                let ran = !facts.candidates.is_empty() || !episodes.candidates.is_empty();
                if !facts.candidates.is_empty() {
                    rankings.push(WeightedRanking::new(profile.weights.importance, facts));
                }
                if !episodes.candidates.is_empty() {
                    rankings.push(WeightedRanking::new(profile.weights.importance, episodes));
                }
                if ran {
                    signals_run.push(Signal::Importance);
                }
            }
            bail_if_past(deadline)?;
            if profile.weights.recency > 0.0 {
                let _signal_span = trace::signal_span(Signal::Recency, fanout).entered();
                let facts = rerank::recency_ranking(&self.store, &fact_set, true)?;
                let episodes = rerank::recency_ranking(&self.store, &episode_set, false)?;
                let ran = !facts.candidates.is_empty() || !episodes.candidates.is_empty();
                if !facts.candidates.is_empty() {
                    rankings.push(WeightedRanking::new(profile.weights.recency, facts));
                }
                if !episodes.candidates.is_empty() {
                    rankings.push(WeightedRanking::new(profile.weights.recency, episodes));
                }
                if ran {
                    signals_run.push(Signal::Recency);
                }
            }
        }

        let signals_ms = signals_started.elapsed().as_millis();
        bail_if_past(deadline)?;

        // 3. Fuse, then resolve, authorize, temporally filter, and diversity-cap. The
        //    reader's visible set was computed up front (it now also scopes episode candidate
        //    generation); every candidate is still gated by the same O(1) membership check
        //    here (06 §1). The admin reveal (07 §4, M6.T02) was folded into `visible` /
        //    `surface_system` above, in lockstep with `include_system`.
        let assemble_started = Instant::now();
        let _assemble_span = trace::stage_span("assemble").entered();
        let fused = fuse(&rankings, DEFAULT_RRF_K);
        // The true candidate pool the selection examined — the distinct fused candidates
        // across every signal, before authorization / temporal / supersession / diversity
        // attrition. This is the honest "considered" count for the explanation: the gap
        // between it and `returned` is the recall attrition an operator needs to see (it was
        // previously reported as ~= returned, hiding it entirely; 03 §6).
        let fused_pool = fused.len();
        // The identity pre-pass (05 §4): every live core block in the reader's
        // visible set is prepended ahead of the ranked results — identity is the
        // standing context a recall is read against, not a hit that competes on
        // relevance, so it bypasses fusion and the diversity cap. The blocks count
        // toward the requested limit (the ranked fill shrinks to make room) but are
        // never themselves capped: a deployment with more identity than limit still
        // gets all of it, honestly, rather than a silent truncation of a redline.
        let core = core_block_entries(&self.store, &visible)?;
        let ranked_budget = query.limit.saturating_sub(core.len());
        let selection = select(
            &self.store,
            &query,
            &visible,
            surface_system,
            fused,
            &fact_nodes,
            ranked_budget,
        )?;

        // 4. Structured view stays in score order behind the identity prefix; the
        //    rendered view re-sorts by serialization id so the same set renders
        //    byte-identically (03 §6).
        let considered = fused_pool + core.len();
        let mut structured = core;
        structured.extend(selection.entries);
        let mut rendered_order = structured.clone();
        // Explicit tie-break by content (itself content-derived, so stable) for the
        // rare case of two entries sharing a serialization id; never by the mint-time
        // domain id, which would not be stable across a rebuild (03 §6).
        rendered_order.sort_by(|a, b| {
            a.serialization_id()
                .cmp(b.serialization_id())
                .then_with(|| a.content().cmp(b.content()))
        });
        let rendered = render(&rendered_order);
        let assemble_ms = assemble_started.elapsed().as_millis();

        let explanation = RecallExplanation {
            class: profile.class,
            weights: profile.weights,
            signals_run,
            embedder_available,
            candidates_considered: considered,
            returned: structured.len(),
            timings_ms: StageTimings {
                classify: classify_ms,
                signals: signals_ms,
                assemble: assemble_ms,
            },
        };

        Ok(RecallBundle {
            structured,
            rendered,
            explanation,
        })
    }
}

impl<E: Embedder> Retriever for HybridRetriever<E> {
    type Query = RecallQuery;
    type Bundle = RecallBundle;
    type Error = RetrievalError;

    fn recall(
        &self,
        query: Self::Query,
    ) -> impl Future<Output = Result<Self::Bundle, Self::Error>> + Send {
        self.run(query)
    }
}
