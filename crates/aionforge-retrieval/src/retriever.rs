//! The hybrid retriever: router → signals → fusion → recall bundle (03).
//!
//! [`HybridRetriever`] implements the domain [`Retriever`] contract. It classifies the
//! query, runs the weighted signals the mode profile calls for, fuses them, authorizes
//! and diversity-caps the candidate set, and assembles the [`RecallBundle`]. It is
//! generic over the [`Embedder`] seam; when the embedder is unreachable the dense
//! signal drops out and retrieval degrades to the rest, flagged in the explanation
//! (03 §6, §8.1).
//!
//! In this milestone only the lexical and dense signals exist, so those are the
//! signals that run; the graph, recency, and trust signals land with their tasks.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use aionforge_domain::contracts::{Embedder, Retriever};
use aionforge_domain::edges::About;
use aionforge_domain::embedding::Embedding;
use aionforge_domain::ids::SerializationId;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{Episode, Role};
use aionforge_domain::nodes::semantic::Fact;
use aionforge_store::{CandidateSet, NodeId, SearchKind, SetOp, Store};

use crate::bundle::{
    EpisodeEntry, FactEntry, RecallBundle, RecallExplanation, StageTimings, StructuredEntry, render,
};
use crate::error::RetrievalError;
use crate::fusion::{DEFAULT_RRF_K, FusedCandidate, WeightedRanking, fuse};
use crate::precision::derive_graph_seed;
use crate::query::{RecallQuery, TemporalMode};
use crate::router::{profile_for, route};
use crate::signals::{
    Signal, SignalRanking, dense_ranking_for, embed_query, lexical_ranking, ranking_from_hits,
};
use crate::temporal::{fact_passes_temporal, fact_serialization_id};

/// The serialization-id kind tag for an episode (02 §10).
const EPISODE_KIND_TAG: &str = "episode";

/// Tuning for the retriever that is not per-query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetrieverConfig {
    /// How many candidates each signal pulls before fusion, when a query does not set
    /// its own fan-out. A wider fan-out gives fusion and the diversity cap more to
    /// work with at the cost of more candidate reads.
    pub default_fanout: usize,
}

impl Default for RetrieverConfig {
    fn default() -> Self {
        Self { default_fanout: 50 }
    }
}

/// A hybrid retriever over a shared store and an embedder.
pub struct HybridRetriever<E> {
    store: Arc<Store>,
    embedder: E,
    config: RetrieverConfig,
}

impl<E: Embedder> HybridRetriever<E> {
    /// Build a retriever over a shared store, an embedder, and its config.
    #[must_use]
    pub fn new(store: Arc<Store>, embedder: E, config: RetrieverConfig) -> Self {
        Self {
            store,
            embedder,
            config,
        }
    }

    /// Run one recall.
    async fn run(&self, query: RecallQuery) -> Result<RecallBundle, RetrievalError> {
        let started = Instant::now();
        let deadline = query.options.deadline.map(|budget| started + budget);

        // 1. Classify (or honor an override).
        let profile = query
            .options
            .mode_override
            .map_or_else(|| route(&query.text), profile_for);
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
            let embedding = embed_query(&self.embedder, &query.text).await;
            embedder_available = embedding.is_some();
            embedding
        } else {
            None
        };

        // In Current mode the fact searches are scoped to the live current-support set.
        // Resolve its membership once: an empty set short-circuits both fact searches, and
        // the non-empty list scopes the lexical BM25 search (there is no
        // `text_score_candidate_state` primitive, so it passes the explicit node list).
        // `None` in any other temporal mode means "search all facts" (03 §5).
        let current_facts: Option<Vec<NodeId>> = match query.options.temporal {
            TemporalMode::Current => Some(
                self.store
                    .candidate_state_members(CandidateSet::CurrentSupportFacts)?,
            ),
            _ => None,
        };

        if profile.weights.lexical > 0.0 {
            let episodes = lexical_ranking(&self.store, SearchKind::Episode, &query.text, fanout)?;
            let facts = self.fact_lexical_ranking(&query, current_facts.as_deref(), fanout)?;
            fact_nodes.extend(facts.candidates.iter().map(|c| c.node));
            rankings.push(WeightedRanking::new(profile.weights.lexical, episodes));
            rankings.push(WeightedRanking::new(profile.weights.lexical, facts));
            signals_run.push(Signal::Lexical);
        }
        bail_if_past(deadline)?;

        if let Some(embedding) = &query_embedding {
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
                query.options.sensitive,
                embedding,
                fanout,
                profile.exact_rerank,
            )?;
            fact_nodes.extend(facts.candidates.iter().map(|c| c.node));
            rankings.push(WeightedRanking::new(profile.weights.dense, episodes));
            rankings.push(WeightedRanking::new(profile.weights.dense, facts));
            signals_run.push(Signal::Dense);
        }
        let signals_ms = signals_started.elapsed().as_millis();
        bail_if_past(deadline)?;

        // 3. Fuse, then resolve, authorize, temporally filter, and diversity-cap.
        let assemble_started = Instant::now();
        let fused = fuse(&rankings, DEFAULT_RRF_K);
        let selection = self.select(&query, fused, &fact_nodes)?;

        // 4. Structured view stays in score order; the rendered view re-sorts by
        //    serialization id so the same set renders byte-identically (03 §6).
        let structured = selection.entries;
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
            candidates_considered: selection.considered,
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
        fused: Vec<FusedCandidate>,
        fact_nodes: &HashSet<NodeId>,
    ) -> Result<Selection, RetrievalError> {
        let cap = query.options.session_diversity_cap;
        let mut primary: Vec<StructuredEntry> = Vec::new();
        let mut spill: Vec<StructuredEntry> = Vec::new();
        let mut per_session: HashMap<Option<String>, usize> = HashMap::new();
        let mut considered = 0usize;

        for candidate in fused {
            if primary.len() >= query.limit {
                break;
            }
            if fact_nodes.contains(&candidate.node) {
                let Some(entry) = self.resolve_fact(query, &candidate)? else {
                    continue;
                };
                considered += 1;
                primary.push(entry);
                continue;
            }
            let Some(episode) = self.store.episode_by_node_id(candidate.node)? else {
                continue;
            };
            if !admit_episode(query, &episode) {
                continue;
            }
            considered += 1;
            let entry = StructuredEntry::Episode(episode_entry(&episode, &candidate));
            let session = episode
                .session_id
                .as_ref()
                .map(|id| id.as_str().to_string());
            let seen = per_session.entry(session).or_insert(0);
            if cap == 0 || *seen < cap {
                *seen += 1;
                primary.push(entry);
            } else {
                spill.push(entry);
            }
        }

        // Under-filled: top up from the spilled overflow, in score order.
        if primary.len() < query.limit {
            for entry in spill {
                if primary.len() >= query.limit {
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
    /// `current` is `Some` in Current mode (the live `current_support_facts` membership)
    /// and `None` otherwise. When a high-precision graph `seed` is present (the factual /
    /// temporal-current path), the ranking is the seed composed with the current-support
    /// set via native `Intersection` set algebra and exact-vector-reranked — the §4
    /// high-precision path that restores current-fact precision a plain ANN pass loses.
    /// `sensitive` swaps in `provenance_current_support_facts` for that composition.
    ///
    /// Without a seed it falls back to T07: an empty current set short-circuits; a
    /// non-empty one is scored through the atomic `vector_score_candidate_state`
    /// primitive (single snapshot, no TOCTOU); a `None` current set runs the standard
    /// ANN-then-rerank path over all facts (temporally filtered per candidate later).
    /// The fact lexical signal always covers the whole current set, so a seed that
    /// resolves the wrong (or no) entity never drops a current fact from recall.
    fn fact_dense_ranking(
        &self,
        current: Option<&[NodeId]>,
        seed: Option<&[NodeId]>,
        sensitive: bool,
        embedding: &Embedding,
        k: usize,
        exact_rerank: bool,
    ) -> Result<SignalRanking, RetrievalError> {
        match current {
            Some([]) => Ok(ranking_from_hits(Signal::Dense, Vec::new())),
            Some(_) => {
                let hits = match seed {
                    Some(seed) if !seed.is_empty() => {
                        let set = if sensitive {
                            CandidateSet::ProvenanceCurrentSupportFacts
                        } else {
                            CandidateSet::CurrentSupportFacts
                        };
                        self.store.vector_score_state_nodes(
                            SearchKind::Fact,
                            embedding,
                            set,
                            seed,
                            SetOp::Intersection,
                            k,
                        )?
                    }
                    _ => self.store.vector_score_state(
                        SearchKind::Fact,
                        embedding,
                        CandidateSet::CurrentSupportFacts,
                        k,
                    )?,
                };
                Ok(ranking_from_hits(Signal::Dense, hits))
            }
            None => dense_ranking_for(&self.store, SearchKind::Fact, embedding, k, exact_rerank),
        }
    }

    /// Resolve a fused fact candidate to an authorized, temporally-admitted entry, or
    /// `None` if it is hidden by namespace, has no validity window, or falls outside the
    /// query's temporal mode (03 §5, §8).
    fn resolve_fact(
        &self,
        query: &RecallQuery,
        candidate: &FusedCandidate,
    ) -> Result<Option<StructuredEntry>, RetrievalError> {
        let Some(fact) = self.store.fact_by_node_id(candidate.node)? else {
            return Ok(None);
        };
        if !visible_to(&query.viewer, &fact.identity.namespace) {
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

/// Whether an episode may surface for this query: not a system-role message (07 §4),
/// active unless history was asked for (03 §5), and visible to the viewer's namespace
/// (03 §8, 06 §1).
fn admit_episode(query: &RecallQuery, episode: &Episode) -> bool {
    if episode.role == Role::System {
        return false;
    }
    if !query.options.include_expired && episode.identity.expired_at.is_some() {
        return false;
    }
    visible_to(&query.viewer, &episode.identity.namespace)
}

/// Namespace authorization: a viewer sees the global namespace and its own; private
/// content from any other namespace never surfaces (06 §1). Team membership is not
/// modeled yet, so a team namespace is visible only to that exact namespace.
fn visible_to(viewer: &Namespace, candidate: &Namespace) -> bool {
    matches!(candidate, Namespace::Global) || candidate == viewer
}

/// Build an episode entry from an episode and its fused candidate.
fn episode_entry(episode: &Episode, candidate: &FusedCandidate) -> EpisodeEntry {
    EpisodeEntry {
        id: episode.identity.id.clone(),
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

/// Build a fact entry from a fact, its `ABOUT` validity window, and its fused candidate.
fn fact_entry(fact: &Fact, about: &About, candidate: &FusedCandidate) -> FactEntry {
    FactEntry {
        id: fact.identity.id.clone(),
        serialization_id: fact_serialization_id(fact),
        namespace: fact.identity.namespace.clone(),
        subject_id: fact.subject_id.clone(),
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
