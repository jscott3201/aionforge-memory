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

use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use aionforge_domain::authz::{Authorizer, DefaultAuthorizer, VisibleSet};
use aionforge_domain::contracts::{Embedder, Retriever};
use aionforge_domain::drift::effective_cooled_trust;
use aionforge_domain::edges::About;
use aionforge_domain::embedding::Embedding;
use aionforge_domain::ids::{ContentHash, SerializationId};
use aionforge_domain::nodes::core::CoreBlock;
use aionforge_domain::nodes::episodic::{Episode, Role};
use aionforge_domain::nodes::semantic::Fact;
use aionforge_domain::time::Timestamp;
use aionforge_store::{
    CandidateSet, ExpandDirection, ExpandEdge, NodeId, SearchKind, SetOp, Store,
};
use tracing::Instrument;

use crate::bundle::{
    CoreBlockEntry, EpisodeEntry, FactEntry, RecallBundle, RecallExplanation, StageTimings,
    StructuredEntry, render,
};
use crate::error::RetrievalError;
use crate::fusion::{DEFAULT_RRF_K, FusedCandidate, WeightedRanking, fuse};
use crate::precision::{derive_graph_seed, resolve_seed_entities};
use crate::query::{RecallQuery, TemporalMode};
use crate::rerank;
use crate::router::{profile_for, route};
use crate::signals::{
    RankedCandidate, Signal, SignalRanking, dense_ranking_for, embed_query, graph_ranking_for,
    lexical_ranking, ranking_from_hits,
};
use crate::temporal::{fact_passes_temporal, fact_serialization_id};
use crate::trace;

/// The serialization-id kind tag for an episode (02 §10).
const EPISODE_KIND_TAG: &str = "episode";

/// The serialization-id kind tag for a core block (02 §10, 05 §4).
const CORE_KIND_TAG: &str = "core";

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
        let deadline = query.options.deadline.map(|budget| started + budget);

        // 1. Classify (or honor an override).
        let profile = trace::stage_span("classify").in_scope(|| {
            query
                .options
                .mode_override
                .map_or_else(|| route(&query.text), profile_for)
        });
        let classify_ms = started.elapsed().as_millis();
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
            let episodes = lexical_ranking(&self.store, SearchKind::Episode, &query.text, fanout)?;
            let facts = self.fact_lexical_ranking(&query, current_facts.as_deref(), fanout)?;
            let lexical_anchor = if profile.weights.lexical_anchor > 0.0 {
                let _anchor_span =
                    trace::signal_span(Signal::LexicalAnchor, LEXICAL_ANCHOR_WINDOW).entered();
                lexical_anchor_ranking(&[&episodes, &facts])
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
            let episodes = dense_ranking_for(
                &self.store,
                SearchKind::Episode,
                embedding,
                fanout,
                profile.exact_rerank,
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
            let facts = self.fact_dense_ranking(
                current_facts.as_deref(),
                graph_seed.as_deref(),
                support_set,
                embedding,
                fanout,
                profile.exact_rerank,
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
            let facts = self.fact_support_ranking(&roots, support_set, embedding, fanout)?;
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
            let episodes = graph_ranking_for(&self.store, SearchKind::Episode, &seeds, fanout)?;
            let facts = self.fact_graph_ranking(&seeds, current_facts.as_deref(), fanout)?;
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
            let (fact_trust, episode_trust) =
                self.trust_rankings(&rankings, &fact_nodes, query.options.now.as_ref())?;
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
        //    reader's visible set is computed once here, through the injected authority,
        //    so every candidate is gated by the same O(1) membership check (06 §1).
        let assemble_started = Instant::now();
        let _assemble_span = trace::stage_span("assemble").entered();
        let fused = fuse(&rankings, DEFAULT_RRF_K);
        // The admin reveal (07 §4, M6.T02): system-role memories surface only when the
        // caller requests it AND the injected authority grants the capability — the request
        // flag alone is inert. When granted, BOTH exclusion gates lift in lockstep: the
        // visible set admits the system namespace, and `admit_episode` skips the role gate.
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
        // The identity pre-pass (05 §4): every live core block in the reader's
        // visible set is prepended ahead of the ranked results — identity is the
        // standing context a recall is read against, not a hit that competes on
        // relevance, so it bypasses fusion and the diversity cap. The blocks count
        // toward the requested limit (the ranked fill shrinks to make room) but are
        // never themselves capped: a deployment with more identity than limit still
        // gets all of it, honestly, rather than a silent truncation of a redline.
        let core = self.core_block_entries(&visible)?;
        let ranked_budget = query.limit.saturating_sub(core.len());
        let selection = self.select(
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
        let considered = selection.considered + core.len();
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

    /// Resolve fused candidates to authorized entries, applying the session-diversity
    /// cap and filling from the spill only when the bundle is under-filled (03 §6).
    ///
    /// A candidate is a fact iff a fact search produced it (`fact_nodes`), else it is
    /// resolved as an episode. The session-diversity cap is an episode notion — it
    /// demotes a conversation that dominates the bundle — so facts, which have no
    /// session, always go straight to the primary set in fused order.
    fn select(
        &self,
        query: &RecallQuery,
        visible: &VisibleSet,
        surface_system: bool,
        fused: Vec<FusedCandidate>,
        fact_nodes: &HashSet<NodeId>,
        limit: usize,
    ) -> Result<Selection, RetrievalError> {
        let cap = query.options.session_diversity_cap;
        let mut primary: Vec<StructuredEntry> = Vec::new();
        let mut spill: Vec<StructuredEntry> = Vec::new();
        let mut per_session: HashMap<Option<String>, usize> = HashMap::new();
        let mut considered = 0usize;

        for candidate in fused {
            if primary.len() >= limit {
                break;
            }
            if fact_nodes.contains(&candidate.node) {
                let Some(entry) = self.resolve_fact(query, visible, &candidate)? else {
                    continue;
                };
                considered += 1;
                primary.push(entry);
                continue;
            }
            let Some(episode) = self.store.episode_by_node_id(candidate.node)? else {
                continue;
            };
            if !admit_episode(query, visible, surface_system, &episode) {
                continue;
            }
            considered += 1;
            let entry = StructuredEntry::Episode(episode_entry(&episode, &candidate));
            let session = episode.session_id.as_ref().map(|id| id.to_string());
            let seen = per_session.entry(session).or_insert(0);
            if cap == 0 || *seen < cap {
                *seen += 1;
                primary.push(entry);
            } else {
                spill.push(entry);
            }
        }

        // Under-filled: top up from the spilled overflow, in score order.
        if primary.len() < limit {
            for entry in spill {
                if primary.len() >= limit {
                    break;
                }
                primary.push(entry);
            }
        }

        Ok(Selection {
            entries: primary,
            considered,
        })
    }

    /// The always-include identity pre-pass (05 §4): every live core block in the
    /// reader's visible set, serialization-id ordered so the prefix is deterministic.
    /// Liveness is the only lifecycle gate — a retired or soft-forgotten block is
    /// already absent from the live scan, and identity is current by definition, so
    /// `include_expired` (a history flag for the ranked tiers) does not resurrect one
    /// here.
    fn core_block_entries(
        &self,
        visible: &VisibleSet,
    ) -> Result<Vec<StructuredEntry>, RetrievalError> {
        let mut entries: Vec<StructuredEntry> = self
            .store
            .live_core_blocks()?
            .into_iter()
            .filter(|block| visible.contains(&block.identity.namespace))
            .map(|block| StructuredEntry::CoreBlock(core_block_entry(&block)))
            .collect();
        // The same content-derived order (with the same content tie-break) as the
        // rendered view, so the prefix is stable across a rebuild (03 §6). Entries
        // that tie on both keys render byte-identically (the serialization id covers
        // every rendered attribute), so whatever order the stable sort leaves them in
        // cannot change the rendered bytes.
        entries.sort_by(|a, b| {
            a.serialization_id()
                .cmp(b.serialization_id())
                .then_with(|| a.content().cmp(b.content()))
        });
        Ok(entries)
    }

    /// The lexical fact ranking, scoped by `current` (03 §1, §5).
    ///
    /// `Some(members)` is the live `current_support_facts` set (Current mode): BM25 runs
    /// over exactly that node list, so a current fact ranked past the fan-out can never be
    /// lost to a fuse-then-filter intersection; an empty set short-circuits the search.
    /// `None` searches all facts and the temporal window is applied per candidate in
    /// [`Self::resolve_fact`].
    fn fact_lexical_ranking(
        &self,
        query: &RecallQuery,
        current: Option<&[NodeId]>,
        k: usize,
    ) -> Result<SignalRanking, RetrievalError> {
        let hits = match current {
            Some([]) => Vec::new(),
            Some(members) => {
                self.store
                    .text_score_nodes(SearchKind::Fact, &query.text, members, k)?
            }
            None => self.store.text_search(SearchKind::Fact, &query.text, k)?,
        };
        Ok(ranking_from_hits(Signal::Lexical, hits))
    }

    /// The dense fact ranking, scoped by `current` and an optional high-precision `seed`
    /// (03 §1, §4, §5).
    ///
    /// `current` is `Some` in Current mode (the live support-set membership) and `None`
    /// otherwise. `support` is the candidate-state set the Current-mode paths read against
    /// — `current_support_facts`, or `provenance_current_support_facts` for a sensitive
    /// query — chosen once by the caller so every fact signal agrees. When a high-precision
    /// graph `seed` is present (the factual / temporal-current path), the ranking is the
    /// seed composed with `support` via native `Intersection` set algebra and
    /// exact-vector-reranked — the §4 high-precision path that restores current-fact
    /// precision a plain ANN pass loses.
    ///
    /// Without a seed it falls back to T07: an empty current set short-circuits; a
    /// non-empty one is scored through the atomic `vector_score_candidate_state`
    /// primitive (single snapshot, no TOCTOU); a `None` current set runs the standard
    /// ANN-then-rerank path over all facts (temporally filtered per candidate later).
    /// The fact lexical signal always covers the whole support set, so a seed that
    /// resolves the wrong (or no) entity never drops a current fact from recall.
    ///
    fn fact_dense_ranking(
        &self,
        current: Option<&[NodeId]>,
        seed: Option<&[NodeId]>,
        support: CandidateSet,
        embedding: &Embedding,
        k: usize,
        exact_rerank: bool,
    ) -> Result<SignalRanking, RetrievalError> {
        match current {
            Some([]) => Ok(ranking_from_hits(Signal::Dense, Vec::new())),
            Some(_) => {
                let hits = if let Some(seed) = seed
                    && !seed.is_empty()
                {
                    self.store.vector_score_state_nodes(
                        SearchKind::Fact,
                        embedding,
                        support,
                        seed,
                        SetOp::Intersection,
                        k,
                    )?
                } else {
                    self.store
                        .vector_score_state(SearchKind::Fact, embedding, support, k)?
                };
                Ok(ranking_from_hits(Signal::Dense, hits))
            }
            None => dense_ranking_for(&self.store, SearchKind::Fact, embedding, k, exact_rerank),
        }
    }

    /// The support-expansion fact ranking (03 §1, §4, M3.T02): the additive
    /// associative-dense signal. The query-entity fact `roots` are expanded one incoming
    /// `SUPPORTS` hop and the roots-plus-evidence set is composed with the `support`
    /// candidate-state set via native `Intersection`, then exact-vector-reranked inside the
    /// store primitive.
    ///
    /// Distinct from the dense signal, which scores every current fact: this scores only the
    /// evidence around the query's entities, so a relevant fact's far-embedded supporting
    /// evidence — which a global ANN pass ranks out of the dense top-k — surfaces with its
    /// own rank, while the dense pass's precision over the rest of the current set is left
    /// untouched. Current-scoped by the `Intersection`, so the reach can never admit a
    /// non-current fact the support provider excludes. The roots are preserved by the
    /// expansion, so a query-entity fact is re-affirmed (dense + support) rather than lost.
    fn fact_support_ranking(
        &self,
        roots: &[NodeId],
        support: CandidateSet,
        embedding: &Embedding,
        k: usize,
    ) -> Result<SignalRanking, RetrievalError> {
        let hits = self.store.vector_score_state_expanded(
            SearchKind::Fact,
            embedding,
            support,
            roots,
            ExpandEdge::Supports,
            ExpandDirection::Incoming,
            SetOp::Intersection,
            k,
        )?;
        Ok(ranking_from_hits(Signal::Support, hits))
    }

    /// The graph (PageRank) fact ranking, scoped by `current` (03 §1, §5).
    ///
    /// PageRank spreads associatively across the whole graph, so — unlike the lexical and
    /// dense fact searches, which the engine bounds to the current-support set — its hits
    /// are not current by construction. In Current mode (`current` is `Some`) they are
    /// intersected with the live support membership here, so graph expansion can never
    /// surface a fact the support provider excludes: `fact_passes_temporal` checks only
    /// `status == active` in Current mode (it trusts the search to have scoped the set), so
    /// a contradicted-but-active fact would otherwise leak in. No current fact is *lost* to
    /// this filter — the lexical fact signal already covers the whole support set, so graph
    /// expansion only adds associative weight to facts the other signals also reach. `None`
    /// (any non-Current mode) leaves every reached fact, with the per-candidate window test
    /// applied later in [`Self::resolve_fact`]. Hits are filtered before they are numbered,
    /// so the surviving ranks stay dense (0, 1, 2, …) for fusion.
    fn fact_graph_ranking(
        &self,
        seeds: &[NodeId],
        current: Option<&[NodeId]>,
        k: usize,
    ) -> Result<SignalRanking, RetrievalError> {
        let hits = self
            .store
            .personalized_pagerank(SearchKind::Fact, seeds, k)?;
        let hits = match current {
            Some(members) => {
                let set: HashSet<NodeId> = members.iter().copied().collect();
                hits.into_iter()
                    .filter(|hit| set.contains(&hit.node))
                    .collect()
            }
            None => hits,
        };
        Ok(ranking_from_hits(Signal::Graph, hits))
    }

    /// Build the per-kind trust re-rankings over the candidates the search signals already
    /// surfaced (03 §1 trust, 06 §5). A candidate is a fact iff a fact search produced it
    /// (`fact_nodes`), so its trust is read from `Fact.stats.trust`; every other candidate is an
    /// episode, read from `Episode.stats.trust`. Returns `(facts, episodes)`, each best-first by
    /// trust. Trust never widens retrieval — it only re-orders what the other signals found.
    fn trust_rankings(
        &self,
        rankings: &[WeightedRanking],
        fact_nodes: &HashSet<NodeId>,
        now: Option<&Timestamp>,
    ) -> Result<(SignalRanking, SignalRanking), RetrievalError> {
        let (facts, episodes) = rerank::surfaced(rankings, fact_nodes);
        Ok((
            self.trust_ranking(&facts, true, now)?,
            self.trust_ranking(&episodes, false, now)?,
        ))
    }

    /// One kind's trust ranking: read each node's stored trust and order it best-first
    /// (highest trust first) under the shared competition rank, so equal-trust candidates
    /// share a position. `is_fact` selects the node reader and the stats field. A node
    /// that no longer resolves is dropped.
    ///
    /// A fact inside its cooling window ranks by its **effective** trust — the stored
    /// scalar times the cooling factor (05 §1, M5.T05) — a pure read-time modulation
    /// that survives a reliability refold and expires when the comparison stops
    /// applying, never via a write. Gated exactly like decay: only when the policy
    /// enables cooling *and* the caller stamped a clock. Episodes never cool.
    fn trust_ranking(
        &self,
        nodes: &BTreeSet<NodeId>,
        is_fact: bool,
        now: Option<&Timestamp>,
    ) -> Result<SignalRanking, RetrievalError> {
        let mut scored: Vec<(NodeId, f64)> = Vec::with_capacity(nodes.len());
        for &node in nodes {
            let trust = if is_fact {
                self.store.fact_by_node_id(node)?.map(|f| match now {
                    Some(now) if self.config.cooling_enabled => effective_cooled_trust(
                        f.stats.trust,
                        f.cooled_until.as_ref(),
                        now,
                        self.config.cooling_factor,
                    ),
                    _ => f.stats.trust,
                })
            } else {
                self.store.episode_by_node_id(node)?.map(|e| e.stats.trust)
            };
            if let Some(trust) = trust {
                scored.push((node, trust));
            }
        }
        Ok(rerank::competition_ranked(Signal::Trust, scored))
    }

    /// Resolve a fused fact candidate to an authorized, temporally-admitted entry, or
    /// `None` if it is hidden by namespace, has no validity window, or falls outside the
    /// query's temporal mode (03 §5, §8).
    fn resolve_fact(
        &self,
        query: &RecallQuery,
        visible: &VisibleSet,
        candidate: &FusedCandidate,
    ) -> Result<Option<StructuredEntry>, RetrievalError> {
        let Some(fact) = self.store.fact_by_node_id(candidate.node)? else {
            return Ok(None);
        };
        if !visible.contains(&fact.identity.namespace) {
            return Ok(None);
        }
        // The soft-forget gate (05 §2, M5.T02): a forgotten fact carries a node-level
        // `expired_at` with its status untouched, so neither the support provider
        // (labels and edges only) nor the temporal predicate (status and the ABOUT
        // window) can see it — this node check is the single exclusion mechanism,
        // mirroring the episode gate. `include_expired` retains the record for history;
        // as-known-at is unaffected by design (it reads the ABOUT edge's transaction
        // window, which a soft-forget never touches).
        if !query.options.include_expired && fact.identity.expired_at.is_some() {
            return Ok(None);
        }
        // The validity window lives on the ABOUT edge, not the node; a fact without one
        // cannot be temporally placed, so it is dropped rather than shown undated.
        let Some(about) = self.store.fact_about(candidate.node)? else {
            return Ok(None);
        };
        if !fact_passes_temporal(&query.options.temporal, &fact, &about) {
            return Ok(None);
        }
        Ok(Some(StructuredEntry::Fact(fact_entry(
            &fact, &about, candidate,
        ))))
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

/// The chosen entries plus how many candidates were considered.
struct Selection {
    entries: Vec<StructuredEntry>,
    considered: usize,
}

/// Build the factual lexical-anchor ranking from the highest lexical hits.
///
/// The anchor is intentionally not a new search: it reuses the already-computed BM25
/// episode and fact rankings, keeps only their top few ranks, and preserves those
/// ranks for fusion. If an exact operational memory was a top lexical hit, fusion can
/// now explain that it stayed high because of both `lexical` and `lexical_anchor`.
fn lexical_anchor_ranking(rankings: &[&SignalRanking]) -> Option<SignalRanking> {
    let mut by_node: HashMap<NodeId, RankedCandidate> = HashMap::new();
    for ranking in rankings {
        for candidate in ranking
            .candidates
            .iter()
            .copied()
            .filter(|candidate| candidate.rank < LEXICAL_ANCHOR_WINDOW)
        {
            by_node
                .entry(candidate.node)
                .and_modify(|existing| {
                    if candidate.rank < existing.rank
                        || (candidate.rank == existing.rank
                            && candidate.score.total_cmp(&existing.score).is_gt())
                    {
                        *existing = candidate;
                    }
                })
                .or_insert(candidate);
        }
    }

    if by_node.is_empty() {
        return None;
    }

    let mut candidates: Vec<RankedCandidate> = by_node.into_values().collect();
    candidates.sort_by(|a, b| a.rank.cmp(&b.rank).then(a.node.cmp(&b.node)));
    Some(SignalRanking {
        signal: Signal::LexicalAnchor,
        candidates,
    })
}

/// True once `deadline` has passed.
fn bail_if_past(deadline: Option<Instant>) -> Result<(), RetrievalError> {
    if deadline.is_some_and(|at| Instant::now() >= at) {
        Err(RetrievalError::DeadlineExceeded)
    } else {
        Ok(())
    }
}

/// Candidates per signal: the query's fan-out, else the configured default, never
/// below the requested bundle size.
fn effective_fanout(query: &RecallQuery, config: &RetrieverConfig) -> usize {
    let base = if query.options.fanout > 0 {
        query.options.fanout
    } else {
        config.default_fanout
    };
    base.max(query.limit).max(1)
}

/// Whether an episode may surface for this query: not a system-role message unless the
/// admin reveal is in force (07 §4), active unless history was asked for (03 §5), and
/// within the reader's visible set (03 §8, 06 §1).
fn admit_episode(
    query: &RecallQuery,
    visible: &VisibleSet,
    surface_system: bool,
    episode: &Episode,
) -> bool {
    if episode.role == Role::System && !surface_system {
        return false;
    }
    if !query.options.include_expired && episode.identity.expired_at.is_some() {
        return false;
    }
    visible.contains(&episode.identity.namespace)
}

/// Build an episode entry from an episode and its fused candidate.
fn episode_entry(episode: &Episode, candidate: &FusedCandidate) -> EpisodeEntry {
    EpisodeEntry {
        id: episode.identity.id,
        serialization_id: SerializationId::derive(
            EPISODE_KIND_TAG,
            episode.content_hash.as_str().as_bytes(),
        ),
        namespace: episode.identity.namespace.clone(),
        role: episode.role,
        ingested_at: episode.identity.ingested_at.clone(),
        expired_at: episode.identity.expired_at.clone(),
        trust: episode.stats.trust,
        score: candidate.score,
        contributions: candidate.contributions.clone(),
        content: episode.content.clone(),
    }
}

/// Build a core-block entry for the identity pre-pass (05 §4). The serialization id
/// derives from everything the full render emits for the block — its kind, its
/// sensitivity, and its content hash — so two blocks render byte-identically exactly
/// when they share a serialization id, and the rendered order needs no fallback to a
/// mint-time id even for same-content blocks of different kinds. An edit moves the
/// block's place in the rendered order exactly when its rendered bytes change.
fn core_block_entry(block: &CoreBlock) -> CoreBlockEntry {
    // The same unit-separator discipline as the fact key; the sensitivity goes in as
    // its canonical JSON so an absent one can never collide with a literal "null".
    let key = format!(
        "{kind}{sep}{sensitivity}{sep}{content}",
        kind = crate::bundle::block_kind_tag(block.block_kind),
        sep = '\u{1f}',
        sensitivity = serde_json::to_string(&block.sensitivity).unwrap_or_default(),
        content = ContentHash::of(block.content.as_bytes()).as_str(),
    );
    CoreBlockEntry {
        id: block.identity.id,
        serialization_id: SerializationId::derive(CORE_KIND_TAG, key.as_bytes()),
        namespace: block.identity.namespace.clone(),
        block_kind: block.block_kind,
        sensitivity: block.sensitivity.clone(),
        trust: block.stats.trust,
        content: block.content.clone(),
    }
}

/// Build a fact entry from a fact, its `ABOUT` validity window, and its fused candidate.
fn fact_entry(fact: &Fact, about: &About, candidate: &FusedCandidate) -> FactEntry {
    FactEntry {
        id: fact.identity.id,
        serialization_id: fact_serialization_id(fact),
        namespace: fact.identity.namespace.clone(),
        subject_id: fact.subject_id,
        predicate: fact.predicate.clone(),
        confidence: fact.confidence,
        status: fact.status,
        trust: fact.stats.trust,
        score: candidate.score,
        contributions: candidate.contributions.clone(),
        statement: fact.statement.clone(),
        ingested_at: about.temporal.ingested_at.clone(),
        expired_at: about.temporal.expired_at.clone(),
        valid_from: about.temporal.valid_from.clone(),
        valid_to: about.temporal.valid_to.clone(),
    }
}
