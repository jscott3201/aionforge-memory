//! Pure retrieval-quality metrics over the public recall bundle.
//!
//! Every function here is a deterministic function of a [`RecallBundle`] (or of an
//! already-extracted ranked id list plus gold labels) — no store, no embedder, no
//! network. That keeps the metrics trivially unit-testable and lets the on-demand
//! sweep runner embed a fixture once, then re-score a whole `min_relevance` sweep with
//! zero extra work.
//!
//! Two families of query are scored:
//! - **positives** (a query with a non-empty gold set) drive [`recall_at_k`],
//!   [`ndcg_at_k`], and the [`false_rejection_rate`] guard;
//! - **negatives** (a query whose correct answer is *empty* — an off-topic query) drive
//!   [`rejection_rate`] via [`is_rejected`].
//!
//! Identity-tier core blocks are excluded everywhere: they reach a bundle through the
//! always-include pre-pass, not the ranked signals, so they are neither a hit nor a
//! rejection.

use std::collections::{HashMap, HashSet};

use aionforge_domain::ids::Id;
use aionforge_retrieval::{RecallBundle, StructuredEntry};

/// The ranked memory ids of a bundle, best-first, with identity core blocks removed.
///
/// The position in the returned vector is the hit's rank (0-based), which is what the
/// ranking metrics consume. Core blocks are dropped because they never competed on
/// relevance.
#[must_use]
pub fn ranked_ids(bundle: &RecallBundle) -> Vec<Id> {
    bundle
        .structured
        .iter()
        .filter(|entry| !matches!(entry, StructuredEntry::CoreBlock(_)))
        .map(|entry| *entry.id())
        .collect()
}

/// Whether a bundle *rejected* its query: it surfaced no ranked hit (no episode or fact).
///
/// This is the off-topic-rejection signal. An identity core block does not count as a
/// hit — a recall that returns only standing identity context, with nothing ranked, has
/// still correctly rejected an off-topic query.
#[must_use]
pub fn is_rejected(bundle: &RecallBundle) -> bool {
    !bundle.structured.iter().any(|entry| {
        matches!(
            entry,
            StructuredEntry::Episode(_) | StructuredEntry::Fact(_)
        )
    })
}

/// Recall@k: the fraction of the gold set that appears in the top `k` ranked ids.
///
/// A vacuous gold set (an off-topic / negative query) returns `1.0` — there is nothing
/// to miss; score such queries with [`rejection_rate`] instead.
#[must_use]
pub fn recall_at_k(ranked: &[Id], gold: &HashSet<Id>, k: usize) -> f64 {
    if gold.is_empty() {
        return 1.0;
    }
    let hits = ranked.iter().take(k).filter(|id| gold.contains(id)).count();
    hits as f64 / gold.len() as f64
}

/// Normalized discounted cumulative gain at `k` for graded relevance.
///
/// `grades` maps a memory id to a relevance grade (`0` = irrelevant); the gain of a hit
/// is `2^grade - 1` and the discount at position `i` (0-based) is `1 / log2(i + 2)`. The
/// ideal DCG is computed from the grades sorted descending. With no relevant memory
/// (every grade `0`, or an empty map) the result is `1.0` when nothing relevant was
/// retrieved (vacuously perfect) and `0.0` otherwise.
#[must_use]
pub fn ndcg_at_k(ranked: &[Id], grades: &HashMap<Id, u8>, k: usize) -> f64 {
    let retrieved = ranked
        .iter()
        .take(k)
        .map(|id| grades.get(id).copied().unwrap_or(0));
    let dcg = discounted_cumulative_gain(retrieved);

    let mut ideal: Vec<u8> = grades
        .values()
        .copied()
        .filter(|grade| *grade > 0)
        .collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    let idcg = discounted_cumulative_gain(ideal.into_iter().take(k));

    if idcg == 0.0 {
        return if dcg == 0.0 { 1.0 } else { 0.0 };
    }
    dcg / idcg
}

/// DCG over an ordered sequence of grades: `sum_i (2^grade_i - 1) / log2(i + 2)`.
fn discounted_cumulative_gain(grades: impl Iterator<Item = u8>) -> f64 {
    grades
        .enumerate()
        .map(|(i, grade)| (2f64.powi(i32::from(grade)) - 1.0) / (i as f64 + 2.0).log2())
        .sum()
}

/// The off-topic-rejection rate: the fraction of negative (off-topic) queries that
/// correctly surfaced no ranked hit.
///
/// Higher is better. An empty slice returns `1.0` (nothing was wrongly admitted).
#[must_use]
pub fn rejection_rate(negatives: &[&RecallBundle]) -> f64 {
    if negatives.is_empty() {
        return 1.0;
    }
    let rejected = negatives
        .iter()
        .filter(|bundle| is_rejected(bundle))
        .count();
    rejected as f64 / negatives.len() as f64
}

/// The false-rejection rate: the fraction of *positive* queries whose entire gold set was
/// dropped from the ranked results.
///
/// This is the guard against an over-aggressive floor — the cost side of off-topic
/// rejection. A positive is counted as false-rejected when none of its gold ids survive
/// in the bundle. An empty slice returns `0.0`. A positive with an empty gold set is
/// skipped (it cannot be false-rejected).
#[must_use]
pub fn false_rejection_rate(positives: &[(&RecallBundle, HashSet<Id>)]) -> f64 {
    if positives.is_empty() {
        return 0.0;
    }
    let dropped = positives
        .iter()
        .filter(|(bundle, gold)| {
            if gold.is_empty() {
                return false;
            }
            let ranked: HashSet<Id> = ranked_ids(bundle).into_iter().collect();
            gold.iter().all(|id| !ranked.contains(id))
        })
        .count();
    dropped as f64 / positives.len() as f64
}

/// A running aggregate of single-answer ranking quality over a query set, generalized
/// from the retrieval crate's fixed-corpus precision check.
///
/// Each query contributes the rank at which its single expected answer was found (or
/// `None` if it was missed); the aggregate then reports the exact-top rate and the mean
/// reciprocal rank. Use it for the single-gold "did the right memory come first" view;
/// use [`recall_at_k`] / [`ndcg_at_k`] for multi-gold graded relevance.
#[derive(Debug, Clone, Default)]
pub struct CorpusMetrics {
    queries: usize,
    exact_top: usize,
    reciprocal_rank_sum: f64,
}

impl CorpusMetrics {
    /// Record one query's outcome: `Some(rank)` (0-based) where its expected answer was
    /// found, or `None` if it was missed.
    pub fn observe(&mut self, rank: Option<usize>) {
        self.queries += 1;
        if let Some(rank) = rank {
            if rank == 0 {
                self.exact_top += 1;
            }
            self.reciprocal_rank_sum += 1.0 / (rank as f64 + 1.0);
        }
    }

    /// How many queries were observed.
    #[must_use]
    pub fn queries(&self) -> usize {
        self.queries
    }

    /// The fraction of queries whose expected answer was the top-ranked hit.
    #[must_use]
    pub fn exact_top_rate(&self) -> f64 {
        if self.queries == 0 {
            0.0
        } else {
            self.exact_top as f64 / self.queries as f64
        }
    }

    /// The mean reciprocal rank over all observed queries (a miss contributes `0`).
    #[must_use]
    pub fn mean_reciprocal_rank(&self) -> f64 {
        if self.queries == 0 {
            0.0
        } else {
            self.reciprocal_rank_sum / self.queries as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use aionforge_domain::ids::{Id, SerializationId};
    use aionforge_domain::namespace::Namespace;
    use aionforge_domain::nodes::episodic::Role;
    use aionforge_domain::time::Timestamp;
    use aionforge_retrieval::{
        EpisodeEntry, QueryClass, RecallBundle, RecallExplanation, SignalWeights, StageTimings,
        StructuredEntry,
    };

    use super::*;

    fn ts() -> Timestamp {
        "2026-01-01T00:00:00Z[UTC]"
            .parse()
            .expect("valid timestamp")
    }

    fn episode(id: Id) -> StructuredEntry {
        StructuredEntry::Episode(EpisodeEntry {
            id,
            serialization_id: SerializationId::derive("episode", id.to_string().as_bytes()),
            namespace: Namespace::Global,
            role: Role::User,
            ingested_at: ts(),
            expired_at: None,
            supersedes: None,
            superseded_by: None,
            trust: 0.5,
            score: 1.0,
            dense_similarity: Some(0.8),
            contributions: Vec::new(),
            content: format!("memory {id}"),
        })
    }

    fn explanation() -> RecallExplanation {
        RecallExplanation {
            class: QueryClass::SingleHopFactual,
            weights: SignalWeights {
                lexical: 1.0,
                lexical_anchor: 1.0,
                dense: 1.0,
                support: 0.0,
                graph: 0.3,
                recency: 0.3,
                importance: 0.3,
                trust: 1.0,
            },
            signals_run: Vec::new(),
            embedder_available: true,
            candidates_considered: 0,
            returned: 0,
            timings_ms: StageTimings::default(),
        }
    }

    fn bundle(ids: &[Id]) -> RecallBundle {
        RecallBundle {
            structured: ids.iter().map(|id| episode(*id)).collect(),
            rendered: String::new(),
            explanation: explanation(),
        }
    }

    #[test]
    fn ranked_ids_preserves_order_and_drops_nothing_for_episodes() {
        let a = Id::generate();
        let b = Id::generate();
        assert_eq!(ranked_ids(&bundle(&[a, b])), vec![a, b]);
    }

    #[test]
    fn is_rejected_only_when_no_ranked_hit() {
        assert!(is_rejected(&bundle(&[])), "an empty bundle is a rejection");
        assert!(
            !is_rejected(&bundle(&[Id::generate()])),
            "a ranked hit is not a rejection"
        );
    }

    #[test]
    fn recall_at_k_counts_gold_in_the_top_k() {
        let a = Id::generate();
        let b = Id::generate();
        let c = Id::generate();
        let ranked = vec![a, b, c];
        let gold: HashSet<Id> = [a, c].into_iter().collect();
        assert!((recall_at_k(&ranked, &gold, 3) - 1.0).abs() < 1e-12);
        assert!(
            (recall_at_k(&ranked, &gold, 1) - 0.5).abs() < 1e-12,
            "only a is in the top 1"
        );
        assert!(
            (recall_at_k(&ranked, &HashSet::new(), 3) - 1.0).abs() < 1e-12,
            "a vacuous gold set scores 1.0"
        );
    }

    #[test]
    fn ndcg_is_one_for_ideal_order_and_lower_for_inverted() {
        let a = Id::generate();
        let b = Id::generate();
        let grades: HashMap<Id, u8> = [(a, 3), (b, 1)].into_iter().collect();
        let ideal = ndcg_at_k(&[a, b], &grades, 2);
        let inverted = ndcg_at_k(&[b, a], &grades, 2);
        assert!((ideal - 1.0).abs() < 1e-12, "ideal order is nDCG 1.0");
        assert!(inverted < ideal, "inverted order scores lower: {inverted}");
        assert!(
            (ndcg_at_k(&[], &HashMap::new(), 2) - 1.0).abs() < 1e-12,
            "no relevant docs retrieved and none exist is vacuously 1.0"
        );
    }

    #[test]
    fn rejection_rate_is_the_fraction_that_returned_empty() {
        let empty = bundle(&[]);
        let hit = bundle(&[Id::generate()]);
        assert!((rejection_rate(&[&empty, &empty]) - 1.0).abs() < 1e-12);
        assert!((rejection_rate(&[&empty, &hit]) - 0.5).abs() < 1e-12);
        assert!(
            (rejection_rate(&[]) - 1.0).abs() < 1e-12,
            "no negatives is vacuously perfect"
        );
    }

    #[test]
    fn false_rejection_rate_flags_positives_whose_gold_was_dropped() {
        let kept = Id::generate();
        let dropped = Id::generate();
        let surfaced = bundle(&[kept]);
        let empty = bundle(&[]);
        let positives = [
            (&surfaced, HashSet::from([kept])),
            (&empty, HashSet::from([dropped])),
        ];
        let refs: Vec<(&RecallBundle, HashSet<Id>)> =
            positives.iter().map(|(b, g)| (*b, g.clone())).collect();
        assert!(
            (false_rejection_rate(&refs) - 0.5).abs() < 1e-12,
            "one of two positives lost its gold"
        );
    }

    #[test]
    fn corpus_metrics_track_exact_top_and_mrr() {
        let mut m = CorpusMetrics::default();
        m.observe(Some(0)); // exact top
        m.observe(Some(1)); // reciprocal 1/2
        m.observe(None); // miss
        assert_eq!(m.queries(), 3);
        assert!((m.exact_top_rate() - 1.0 / 3.0).abs() < 1e-12);
        assert!((m.mean_reciprocal_rank() - (1.0 + 0.5) / 3.0).abs() < 1e-12);
    }
}
