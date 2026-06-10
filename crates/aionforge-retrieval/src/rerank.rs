//! The importance and recency re-rank builders, and the competition-rank core every
//! re-rank signal shares (03 §1, 05 §2, M5.T01).
//!
//! A re-rank is a quality signal, never a retrieval: it orders **only** the candidates the
//! search signals already surfaced, per kind, and folds that ordering into reciprocal-rank
//! fusion like any other ranking — it can sink or lift within the relevant set but can
//! never widen it. Importance orders by the *effective* (decayed) importance computed at
//! rank time from the caller-supplied `now` — the stored value is read, never rewritten
//! (§13.7) — and recency orders by the immutable ingestion instant, newest first. Both are
//! built only when the caller stamped [`RecallOptions::now`](crate::RecallOptions): there
//! is no ambient clock in the retrieval path.
//!
//! The competition rank is load-bearing: candidates with equal scores share a position
//! (0, 1, 1, 3, …), so a uniform set collapses to one rank, adds the same constant to every
//! candidate in fusion, and reorders nothing. An ordinal rank here would turn ties into a
//! node-id-ordered bias — the regression the trust re-rank's diversity tests caught.

use std::collections::{BTreeSet, HashSet};

use aionforge_domain::decay::{Tier, decayed_importance};
use aionforge_domain::time::Timestamp;
use aionforge_store::{NodeId, Store};

use crate::error::RetrievalError;
use crate::fusion::WeightedRanking;
use crate::retriever::RetrieverConfig;
use crate::signals::{RankedCandidate, Signal, SignalRanking};

/// Split every candidate the prior rankings surfaced into the fact set and the episode
/// set. A candidate is a fact iff a fact search produced it (`fact_nodes`); everything
/// else reads as an episode and a node that resolves as neither is dropped by the builder
/// that reads it.
pub(crate) fn surfaced(
    rankings: &[WeightedRanking],
    fact_nodes: &HashSet<NodeId>,
) -> (BTreeSet<NodeId>, BTreeSet<NodeId>) {
    let mut facts: BTreeSet<NodeId> = BTreeSet::new();
    let mut episodes: BTreeSet<NodeId> = BTreeSet::new();
    for weighted in rankings {
        for candidate in &weighted.ranking.candidates {
            if fact_nodes.contains(&candidate.node) {
                facts.insert(candidate.node);
            } else {
                episodes.insert(candidate.node);
            }
        }
    }
    (facts, episodes)
}

/// Order `scored` best-first (highest score first) under `signal`, with a *competition*
/// rank so equal-score candidates share a position.
///
/// Ties order by node id so the result is deterministic, and the rank boundary uses the
/// same convention the sort used (a non-comparable pair counts as equal), so the rank
/// never disagrees with the order. A uniform-score set collapses to one rank: in
/// reciprocal-rank fusion the signal then adds the same constant to every candidate and
/// reorders nothing — a re-rank only moves candidates where its values genuinely differ.
pub(crate) fn competition_ranked(signal: Signal, mut scored: Vec<(NodeId, f64)>) -> SignalRanking {
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    let mut candidates = Vec::with_capacity(scored.len());
    let mut rank = 0;
    for (i, &(node, score)) in scored.iter().enumerate() {
        if i > 0
            && scored[i - 1]
                .1
                .partial_cmp(&score)
                .unwrap_or(std::cmp::Ordering::Equal)
                != std::cmp::Ordering::Equal
        {
            rank = i;
        }
        candidates.push(RankedCandidate { node, rank, score });
    }
    SignalRanking { signal, candidates }
}

/// One kind's effective-importance ranking (05 §2): each node's stored importance sunk by
/// the per-tier exponential decay since its `last_access`, at the caller-supplied `now`,
/// ordered best-first. The decayed value orders this ranking and is never written back.
///
/// The tier follows the kind split the re-rank already has: a fact is semantic memory, an
/// episode is episodic. With decay disabled the half-life is inert and the ranking orders
/// by the raw stored importance — the signal still participates; the switch governs only
/// whether elapsed time moves the value. A pinned node keeps its full stored importance
/// (the pin short-circuit in [`decayed_importance`]). A node that no longer resolves is
/// dropped.
pub(crate) fn importance_ranking(
    store: &Store,
    nodes: &BTreeSet<NodeId>,
    is_fact: bool,
    now: &Timestamp,
    config: &RetrieverConfig,
) -> Result<SignalRanking, RetrievalError> {
    let tier = if is_fact {
        Tier::Semantic
    } else {
        Tier::Episodic
    };
    let half_life = half_life_for(tier, config);
    let mut scored: Vec<(NodeId, f64)> = Vec::with_capacity(nodes.len());
    for &node in nodes {
        let stats = if is_fact {
            store.fact_by_node_id(node)?.map(|f| f.stats)
        } else {
            store.episode_by_node_id(node)?.map(|e| e.stats)
        };
        if let Some(stats) = stats {
            let effective = decayed_importance(
                stats.importance,
                &stats.last_access,
                now,
                half_life,
                stats.is_pinned,
            );
            scored.push((node, effective));
        }
    }
    Ok(competition_ranked(Signal::Importance, scored))
}

/// One kind's recency ranking (03 §1): order by the immutable ingestion instant, newest
/// first. The score is the instant's whole-second value, so equal-instant candidates share
/// a competition rank. A node that no longer resolves is dropped.
///
/// Ingestion time deliberately differs from the importance signal's clock: `ingested_at`
/// is *when the substrate learned it*, while decay elapses from `last_access` — using one
/// for both would double-count a single time axis.
pub(crate) fn recency_ranking(
    store: &Store,
    nodes: &BTreeSet<NodeId>,
    is_fact: bool,
) -> Result<SignalRanking, RetrievalError> {
    let mut scored: Vec<(NodeId, f64)> = Vec::with_capacity(nodes.len());
    for &node in nodes {
        let ingested_at = if is_fact {
            store.fact_by_node_id(node)?.map(|f| f.identity.ingested_at)
        } else {
            store
                .episode_by_node_id(node)?
                .map(|e| e.identity.ingested_at)
        };
        if let Some(at) = ingested_at {
            // Whole seconds of the underlying instant: zone-representation-robust, and
            // f64-exact for any realistic epoch second.
            #[allow(clippy::cast_precision_loss)]
            scored.push((node, at.timestamp().as_second() as f64));
        }
    }
    Ok(competition_ranked(Signal::Recency, scored))
}

/// The half-life the importance re-rank decays a tier by, in seconds. Inert (`0.0`, no
/// decay — [`decayed_importance`] returns the stored value) when decay is disabled.
fn half_life_for(tier: Tier, config: &RetrieverConfig) -> f64 {
    if !config.decay_enabled {
        return 0.0;
    }
    match tier {
        Tier::Episodic => config.episodic_half_life_secs,
        Tier::Semantic => config.semantic_half_life_secs,
    }
}
