//! The lexical and dense retrieval signals (03 §1).
//!
//! Each signal turns a query into a ranked, best-first candidate list. The lists
//! feed reciprocal-rank fusion (03 §2), which is rank-based — it consumes each
//! candidate's position, not its raw engine score — so a BM25 score and a cosine
//! distance never have to be made comparable. The raw score rides along for the
//! retrieval explanation (03 §6).
//!
//! Candidates are carried as the store's `NodeId` handle: it is the currency the
//! engine's native candidate-set algebra and the fusion stage work in, resolved to a
//! stable domain id only at the recall-bundle boundary. The dense signal degrades
//! when the embedder is unreachable — an empty ranking with `embedder_available`
//! false — so retrieval falls back to the remaining signals (03 §6).

use std::time::Instant;

use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::Embedding;
use aionforge_domain::namespace::Namespace;
use aionforge_store::{NodeId, SearchHit, SearchKind, Store};

use crate::error::RetrievalError;

/// A retrieval signal — the source that produced a ranking (03 §1). This module
/// implements lexical and dense; graph, recency, importance, and trust land with
/// their tasks' re-rank builders.
///
/// The declared order (lexical, lexical anchor, dense, …) is the canonical order fusion sums
/// contributions in, so a fused result is independent of the order signals are
/// supplied (03 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Signal {
    /// Native BM25 over a maintained text index.
    Lexical,
    /// Factual-query boost for the highest lexical hits. This is derived from the BM25
    /// rankings and exists so an exact operational memory does not sink under several
    /// weak quality re-ranks from adjacent memories.
    LexicalAnchor,
    /// Native vector search, optionally exact-reranked.
    Dense,
    /// Graph-guided dense scoring over a query entity's supporting evidence (03 §1, §4,
    /// M3.T02): the query-entity fact roots expanded one incoming `SUPPORTS` hop,
    /// vector-scored and current-scoped. Additive to [`Signal::Dense`] — it recovers a
    /// relevant fact's far-embedded evidence without narrowing the dense pass.
    Support,
    /// Associative graph expansion.
    Graph,
    /// Recency ranking over ingestion time.
    Recency,
    /// Effective-importance ranking over the decayed write-time importance (05 §2,
    /// M5.T01).
    Importance,
    /// Writer-trust × reliability ranking.
    Trust,
}

/// One candidate in a signal's ranked list. `rank` is the 0-based best-first
/// position — the value fusion consumes; `score` is the raw engine score, kept for
/// the explanation but not used by rank fusion.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RankedCandidate {
    /// The matched node.
    pub node: NodeId,
    /// Best-first position, starting at 0.
    pub rank: usize,
    /// The raw engine score that ordered it (BM25 score or cosine distance).
    pub score: f64,
}

/// A single signal's ranked candidate list, best-first.
#[derive(Debug, Clone, PartialEq)]
pub struct SignalRanking {
    /// Which signal produced this list.
    pub signal: Signal,
    /// The candidates, best-first (`rank` ascending).
    pub candidates: Vec<RankedCandidate>,
}

/// A dense ranking plus whether the embedder was reachable. When it was not, the
/// ranking is empty and `embedder_available` is false, the graceful-degradation
/// signal the fusion stage uses to renormalize weights (03 §6).
#[derive(Debug, Clone, PartialEq)]
pub struct DenseRanking {
    /// The dense candidate list (empty when the embedder was unavailable).
    pub ranking: SignalRanking,
    /// Whether the query embedding was produced.
    pub embedder_available: bool,
}

/// Rank a kind's text index against the query with native BM25 (03 §1 lexical).
///
/// # Errors
/// Returns [`RetrievalError`] if `kind` has no text index, or the search fails.
pub fn lexical_ranking(
    store: &Store,
    kind: SearchKind,
    query: &str,
    k: usize,
    deadline: Option<Instant>,
) -> Result<SignalRanking, RetrievalError> {
    let hits = store.text_search_within(kind, query, k, deadline)?;
    Ok(ranking_from_hits(Signal::Lexical, hits))
}

/// Rank a kind by dense similarity to the query (03 §1 dense).
///
/// Embeds the query, runs approximate vector search, and — when `exact_rerank` is
/// set — refines the order of the retrieved set with full-precision scoring (the
/// HNSW-then-Flat-oracle path, 03 §1, §4). An unreachable embedder degrades to an
/// empty ranking rather than an error (03 §6).
///
/// # Errors
/// Returns [`RetrievalError`] if a search fails. Embedder unavailability is reported
/// on [`DenseRanking::embedder_available`], not as an error.
pub async fn dense_ranking<E: Embedder>(
    store: &Store,
    embedder: &E,
    kind: SearchKind,
    query: &str,
    k: usize,
    exact_rerank: bool,
    deadline: Option<Instant>,
) -> Result<DenseRanking, RetrievalError> {
    let Some(embedding) = embed_query(embedder, query).await else {
        return Ok(DenseRanking {
            ranking: ranking_from_hits(Signal::Dense, Vec::new()),
            embedder_available: false,
        });
    };
    let hits = dense_hits(store, kind, &embedding, k, exact_rerank, deadline)?;
    Ok(DenseRanking {
        ranking: ranking_from_hits(Signal::Dense, hits),
        embedder_available: true,
    })
}

/// A dense ranking over a kind from a query vector that has already been embedded.
///
/// The body the hybrid retriever uses once it has embedded the query a single time and
/// fans the same vector across the kinds it searches (episodes and facts), so a recall
/// never embeds the query twice. Embedder availability is decided at the embed step;
/// this is the pure search half (03 §1).
///
/// # Errors
/// Returns [`RetrievalError`] if a search fails.
pub(crate) fn dense_ranking_for(
    store: &Store,
    kind: SearchKind,
    embedding: &Embedding,
    k: usize,
    exact_rerank: bool,
    deadline: Option<Instant>,
) -> Result<SignalRanking, RetrievalError> {
    Ok(ranking_from_hits(
        Signal::Dense,
        dense_hits(store, kind, embedding, k, exact_rerank, deadline)?,
    ))
}

/// Rank a kind by associative proximity to seed entities, via native Personalized
/// PageRank (03 §1 graph), optionally SCOPED to an explicit candidate node set. Mass
/// restarts on the `seeds` (the entities the query names) and spreads across the associative
/// graph — `MENTIONS`/`ABOUT`/`SUPPORTS` — so the returned nodes are the `kind` instances
/// closest to those entities. Best-first by PageRank score; rank fusion reads only the
/// position, so the score scale never has to be reconciled with the cosine/BM25 signals.
///
/// `result_nodes` is `Some(scope)` to restrict the ranking to the reader's visible-namespace
/// records of `kind` (the episode side, 03 §6 namespace scoping): the scope intersects inside
/// the procedure before the top-`k` truncation, so the graph fan-out lands on in-scope
/// episodes instead of a cross-namespace top-`k` that `select` would mostly discard. `None`
/// ranks over the whole projection (the fact side, which the retriever current-scopes after
/// fusion — a PageRank reach is not bounded to the current-support set the way the
/// lexical/dense fact searches are).
///
/// # Errors
/// Returns [`RetrievalError`] if the PageRank call fails.
pub(crate) fn graph_ranking_for(
    store: &Store,
    kind: SearchKind,
    seeds: &[NodeId],
    k: usize,
    result_nodes: Option<&[NodeId]>,
    deadline: Option<Instant>,
) -> Result<SignalRanking, RetrievalError> {
    Ok(ranking_from_hits(
        Signal::Graph,
        store.personalized_pagerank_within(kind, seeds, k, result_nodes, deadline)?,
    ))
}

/// Rank a kind's text index against the query with native BM25, SCOPED to the reader's
/// visible namespaces (03 §1 lexical, §6 namespace scoping).
///
/// The namespace-scoped episode counterpart of [`lexical_ranking`]: it pushes a
/// `namespace IN namespaces` predicate into the BM25 scan (selene-db 1.3
/// `filter_property`/`filter_values`) so only the reader's visible-namespace records are
/// scored, and the top-`k` come straight from the index — no scope node-set is materialized
/// first. BM25 is exact either way; the predicate narrows which nodes are scored. An empty
/// `namespaces` short-circuits to an empty ranking (the reader has no scope), avoiding an
/// empty-filter engine call.
///
/// `deadline` bounds the scan: the visible scope can span an entire busy namespace, so —
/// unlike the current-support-bounded fact lexical path — this scan needs the same
/// cancellation budget the unscoped [`lexical_ranking`] carries (03 §6, §8).
///
/// # Errors
/// Returns [`RetrievalError`] if the scoped BM25 search fails.
pub(crate) fn lexical_ranking_in_namespaces(
    store: &Store,
    kind: SearchKind,
    query: &str,
    namespaces: &[Namespace],
    k: usize,
    deadline: Option<Instant>,
) -> Result<SignalRanking, RetrievalError> {
    if namespaces.is_empty() {
        return Ok(ranking_from_hits(Signal::Lexical, Vec::new()));
    }
    let hits = store.text_search_in_namespaces(kind, query, namespaces, k, deadline)?;
    Ok(ranking_from_hits(Signal::Lexical, hits))
}

/// Rank a kind by dense similarity to the query, SCOPED to the reader's visible namespaces
/// via predicate-filtered ANN search (03 §1 dense, §6 namespace scoping).
///
/// The namespace-scoped episode counterpart of [`dense_ranking_for`]: it pushes a
/// `namespace IN namespaces` predicate into the ANN traversal (selene-db 1.3
/// `filter_property`/`filter_values`), so the engine admits only visible-namespace records
/// during the HNSW/IVF walk and reranks the survivors with exact cosine — the fan-out lands on
/// in-scope records without first materializing the scope node-set. Unlike the prior
/// exact-over-the-whole-scope path, this is an ANN search (approximate candidate generation,
/// exact-cosine rerank), so recall rides the engine's filtered-ANN quality. An empty
/// `namespaces` short-circuits to an empty ranking.
///
/// # Errors
/// Returns [`RetrievalError`] if the scoped vector search fails.
pub(crate) fn dense_ranking_in_namespaces(
    store: &Store,
    kind: SearchKind,
    embedding: &Embedding,
    namespaces: &[Namespace],
    k: usize,
    deadline: Option<Instant>,
) -> Result<SignalRanking, RetrievalError> {
    if namespaces.is_empty() {
        return Ok(ranking_from_hits(Signal::Dense, Vec::new()));
    }
    let hits = store.vector_search_ann_in_namespaces(kind, embedding, namespaces, k, deadline)?;
    Ok(ranking_from_hits(Signal::Dense, hits))
}

/// Run approximate vector search and, when `exact_rerank` is set, refine the retrieved
/// set with full-precision scoring (the HNSW-then-Flat-oracle path, 03 §1, §4).
fn dense_hits(
    store: &Store,
    kind: SearchKind,
    embedding: &Embedding,
    k: usize,
    exact_rerank: bool,
    deadline: Option<Instant>,
) -> Result<Vec<SearchHit>, RetrievalError> {
    let approximate = store.vector_search_ann_within(kind, embedding, k, deadline)?;
    let hits = if exact_rerank && !approximate.is_empty() {
        let candidates: Vec<NodeId> = approximate.iter().map(|hit| hit.node).collect();
        store.vector_rerank_within(kind, embedding, &candidates, k, deadline)?
    } else {
        approximate
    };
    Ok(hits)
}

/// Embed the query, returning `None` if the embedder is unreachable or returns no
/// vector — the caller treats that as graceful degradation, not failure.
pub(crate) async fn embed_query<E: Embedder>(embedder: &E, query: &str) -> Option<Embedding> {
    let inputs = [query.to_string()];
    embedder.embed(&inputs).await.ok()?.into_iter().next()
}

/// Number a kind's engine hits into a best-first ranking. The retriever uses this to
/// wrap the scoped fact searches (BM25 over a candidate-state node list, vector scoring
/// over a maintained set) it composes outside the generic signal helpers.
pub(crate) fn ranking_from_hits(signal: Signal, hits: Vec<SearchHit>) -> SignalRanking {
    let candidates = hits
        .into_iter()
        .enumerate()
        .map(|(rank, hit)| RankedCandidate {
            node: hit.node,
            rank,
            score: hit.score,
        })
        .collect();
    SignalRanking { signal, candidates }
}
