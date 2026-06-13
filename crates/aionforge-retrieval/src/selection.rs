use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Instant;

use aionforge_domain::authz::VisibleSet;
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

use crate::bundle::{CoreBlockEntry, EpisodeEntry, FactEntry, StructuredEntry};
use crate::error::RetrievalError;
use crate::fusion::{FusedCandidate, WeightedRanking};
use crate::query::RecallQuery;
use crate::rerank;
use crate::retriever::RetrieverConfig;
use crate::signals::{
    RankedCandidate, Signal, SignalRanking, dense_ranking_for, ranking_from_hits,
};
use crate::temporal::{fact_passes_temporal, fact_serialization_id};

/// The serialization-id kind tag for an episode (02 §10).
const EPISODE_KIND_TAG: &str = "episode";

/// The serialization-id kind tag for a core block (02 §10, 05 §4).
const CORE_KIND_TAG: &str = "core";

/// The chosen entries plus how many candidates were considered.
pub(crate) struct Selection {
    pub(crate) entries: Vec<StructuredEntry>,
    pub(crate) considered: usize,
}

enum ResolvedCandidate {
    Fact(FusedCandidate),
    Episode {
        candidate: FusedCandidate,
        episode: Box<Episode>,
    },
}

/// Build the factual lexical-anchor ranking from the highest lexical hits.
///
/// The anchor is intentionally not a new search: it reuses the already-computed BM25
/// episode and fact rankings, keeps only their top few ranks, and preserves those
/// ranks for fusion. If an exact operational memory was a top lexical hit, fusion can
/// now explain that it stayed high because of both `lexical` and `lexical_anchor`.
pub(crate) fn lexical_anchor_ranking(
    rankings: &[&SignalRanking],
    window: usize,
) -> Option<SignalRanking> {
    let mut by_node: HashMap<NodeId, RankedCandidate> = HashMap::new();
    for ranking in rankings {
        for candidate in ranking
            .candidates
            .iter()
            .copied()
            .filter(|candidate| candidate.rank < window)
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
pub(crate) fn bail_if_past(deadline: Option<Instant>) -> Result<(), RetrievalError> {
    if deadline.is_some_and(|at| Instant::now() >= at) {
        Err(RetrievalError::DeadlineExceeded)
    } else {
        Ok(())
    }
}

/// Candidates per signal: the query's fan-out, else the configured default, never
/// below the requested bundle size.
pub(crate) fn effective_fanout(query: &RecallQuery, config: &RetrieverConfig) -> usize {
    let base = if query.options.fanout > 0 {
        query.options.fanout
    } else {
        config.default_fanout
    };
    base.max(query.limit).max(1)
}

/// Resolve fused candidates to authorized entries, applying the session-diversity
/// cap and filling from the spill only when the bundle is under-filled (03 §6).
///
/// A candidate is a fact iff a fact search produced it (`fact_nodes`), else it is
/// resolved as an episode. The session-diversity cap is an episode notion — it
/// demotes a conversation that dominates the bundle — so facts, which have no
/// session, always go straight to the primary set in fused order.
pub(crate) fn select(
    store: &Store,
    query: &RecallQuery,
    visible: &VisibleSet,
    surface_system: bool,
    fused: Vec<FusedCandidate>,
    fact_nodes: &HashSet<NodeId>,
    limit: usize,
) -> Result<Selection, RetrievalError> {
    if limit == 0 {
        return Ok(Selection {
            entries: Vec::new(),
            considered: 0,
        });
    }
    let cap = query.options.session_diversity_cap;
    let mut primary: Vec<StructuredEntry> = Vec::new();
    let mut spill: Vec<StructuredEntry> = Vec::new();
    let mut per_session: HashMap<Option<String>, usize> = HashMap::new();
    let mut considered = 0usize;
    let mut resolved = Vec::new();
    let mut episode_ids = Vec::new();

    for candidate in fused {
        if fact_nodes.contains(&candidate.node) {
            resolved.push(ResolvedCandidate::Fact(candidate));
            continue;
        }
        let Some(episode) = store.episode_by_node_id(candidate.node)? else {
            continue;
        };
        if !admit_episode(query, visible, surface_system, &episode) {
            continue;
        }
        episode_ids.push(episode.identity.id);
        resolved.push(ResolvedCandidate::Episode {
            candidate,
            episode: Box::new(episode),
        });
    }

    let superseded_by = store.live_episode_superseded_by_many(episode_ids.iter())?;

    for candidate in resolved {
        if primary.len() >= limit {
            break;
        }
        match candidate {
            ResolvedCandidate::Fact(candidate) => {
                let Some(entry) = resolve_fact(store, query, visible, &candidate)? else {
                    continue;
                };
                considered += 1;
                primary.push(entry);
                continue;
            }
            ResolvedCandidate::Episode { candidate, episode } => {
                let replacement = superseded_by.get(&episode.identity.id).copied();
                if !query.options.include_superseded && replacement.is_some() {
                    continue;
                }
                considered += 1;
                let entry =
                    StructuredEntry::Episode(episode_entry(&episode, &candidate, replacement));
                let session = episode.session_id.as_ref().map(|id| id.to_string());
                let seen = per_session.entry(session).or_insert(0);
                if cap == 0 || *seen < cap {
                    *seen += 1;
                    primary.push(entry);
                } else {
                    spill.push(entry);
                }
            }
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
pub(crate) fn core_block_entries(
    store: &Store,
    visible: &VisibleSet,
) -> Result<Vec<StructuredEntry>, RetrievalError> {
    let mut entries: Vec<StructuredEntry> = store
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
/// `resolve_fact`.
pub(crate) fn fact_lexical_ranking(
    store: &Store,
    query: &RecallQuery,
    current: Option<&[NodeId]>,
    k: usize,
    deadline: Option<Instant>,
) -> Result<SignalRanking, RetrievalError> {
    // The deadline bounds only the unscoped full-index BM25 fallback; the scoped
    // `text_score_nodes` runs over a bounded current-support member set (fast).
    let hits = match current {
        Some([]) => Vec::new(),
        Some(members) => store.text_score_nodes(SearchKind::Fact, &query.text, members, k)?,
        None => store.text_search_within(SearchKind::Fact, &query.text, k, deadline)?,
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn fact_dense_ranking(
    store: &Store,
    current: Option<&[NodeId]>,
    seed: Option<&[NodeId]>,
    support: CandidateSet,
    embedding: &Embedding,
    k: usize,
    exact_rerank: bool,
    deadline: Option<Instant>,
) -> Result<SignalRanking, RetrievalError> {
    match current {
        Some([]) => Ok(ranking_from_hits(Signal::Dense, Vec::new())),
        // The scoped candidate-state vector scoring runs over a bounded support set,
        // so it is fast and not deadline-bounded; only the unscoped ANN fallback below is.
        Some(_) => {
            let hits = if let Some(seed) = seed
                && !seed.is_empty()
            {
                store.vector_score_state_nodes(
                    SearchKind::Fact,
                    embedding,
                    support,
                    seed,
                    SetOp::Intersection,
                    k,
                )?
            } else {
                store.vector_score_state(SearchKind::Fact, embedding, support, k)?
            };
            Ok(ranking_from_hits(Signal::Dense, hits))
        }
        None => dense_ranking_for(
            store,
            SearchKind::Fact,
            embedding,
            k,
            exact_rerank,
            deadline,
        ),
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
pub(crate) fn fact_support_ranking(
    store: &Store,
    roots: &[NodeId],
    support: CandidateSet,
    embedding: &Embedding,
    k: usize,
) -> Result<SignalRanking, RetrievalError> {
    let hits = store.vector_score_state_expanded(
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
/// applied later in `resolve_fact`. Hits are filtered before they are numbered, so the
/// surviving ranks stay dense (0, 1, 2, …) for fusion.
pub(crate) fn fact_graph_ranking(
    store: &Store,
    seeds: &[NodeId],
    current: Option<&[NodeId]>,
    k: usize,
    deadline: Option<Instant>,
) -> Result<SignalRanking, RetrievalError> {
    let hits = store.personalized_pagerank_within(SearchKind::Fact, seeds, k, deadline)?;
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
pub(crate) fn trust_rankings(
    store: &Store,
    config: &RetrieverConfig,
    rankings: &[WeightedRanking],
    fact_nodes: &HashSet<NodeId>,
    now: Option<&Timestamp>,
) -> Result<(SignalRanking, SignalRanking), RetrievalError> {
    let (facts, episodes) = rerank::surfaced(rankings, fact_nodes);
    Ok((
        trust_ranking(store, config, &facts, true, now)?,
        trust_ranking(store, config, &episodes, false, now)?,
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
    store: &Store,
    config: &RetrieverConfig,
    nodes: &BTreeSet<NodeId>,
    is_fact: bool,
    now: Option<&Timestamp>,
) -> Result<SignalRanking, RetrievalError> {
    let mut scored: Vec<(NodeId, f64)> = Vec::with_capacity(nodes.len());
    for &node in nodes {
        let trust = if is_fact {
            store.fact_by_node_id(node)?.map(|f| match now {
                Some(now) if config.cooling_enabled => effective_cooled_trust(
                    f.stats.trust,
                    f.cooled_until.as_ref(),
                    now,
                    config.cooling_factor,
                ),
                _ => f.stats.trust,
            })
        } else {
            store.episode_by_node_id(node)?.map(|e| e.stats.trust)
        };
        if let Some(trust) = trust {
            scored.push((node, trust));
        }
    }
    Ok(rerank::competition_ranked(Signal::Trust, scored))
}

fn admit_episode(
    query: &RecallQuery,
    visible: &VisibleSet,
    surface_system: bool,
    episode: &Episode,
) -> bool {
    if !surface_system && episode.role == Role::System {
        return false;
    }
    if !visible.contains(&episode.identity.namespace) {
        return false;
    }
    query.options.include_expired || episode.identity.expired_at.is_none()
}

fn episode_entry(
    episode: &Episode,
    candidate: &FusedCandidate,
    superseded_by: Option<aionforge_domain::ids::Id>,
) -> EpisodeEntry {
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
        supersedes: episode.origin.as_ref().and_then(|origin| origin.supersedes),
        superseded_by,
        trust: episode.stats.trust,
        score: candidate.score,
        contributions: candidate.contributions.clone(),
        content: episode.content.clone(),
    }
}

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
        content: block.content.clone(),
        block_kind: block.block_kind,
        sensitivity: block.sensitivity.clone(),
        trust: block.stats.trust,
    }
}

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

/// Resolve a fused fact candidate to an authorized, temporally-admitted entry, or
/// `None` if it is hidden by namespace, has no validity window, or falls outside the
/// query's temporal mode (03 §5, §8).
fn resolve_fact(
    store: &Store,
    query: &RecallQuery,
    visible: &VisibleSet,
    candidate: &FusedCandidate,
) -> Result<Option<StructuredEntry>, RetrievalError> {
    let Some(fact) = store.fact_by_node_id(candidate.node)? else {
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
    let Some(about) = store.fact_about(candidate.node)? else {
        return Ok(None);
    };
    if !fact_passes_temporal(&query.options.temporal, &fact, &about) {
        return Ok(None);
    }
    Ok(Some(StructuredEntry::Fact(fact_entry(
        &fact, &about, candidate,
    ))))
}
