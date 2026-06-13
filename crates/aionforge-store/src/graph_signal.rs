//! Native Personalized PageRank as an associative retrieval signal (03 §1 graph, M3.T01).
//!
//! Seeds PageRank on the entities a query mentions and reads back the facts and episodes
//! that sit closest to them in the associative graph — `Episode -MENTIONS-> Entity`,
//! `Fact -ABOUT-> Entity`, and `Fact|Episode -SUPPORTS-> Fact`. Retrieval (L1) gates this
//! to the classes that benefit (multi-hop, entity) and fuses the per-kind ranking with the
//! others.
//!
//! ## Why undirected
//! The associative schema points records *at* entities, so under natural directed PageRank
//! an entity is a sink: personalized mass seeded on a query-mention entity cannot reach the
//! facts and episodes that reference it. The signal runs PageRank with selene's `undirected`
//! orientation so the seed's mass spreads across incident edges regardless of stored
//! direction — entity ↔ fact ↔ fact — which is the associative prior this signal is for.
//!
//! ## Why one session
//! A selene graph projection lives in the *session*, not the graph, so the projection and
//! the PageRank over it must run in a single session — hence [`Store::execute_session`]
//! rather than two [`Store::execute`] calls. The projection is ephemeral: it is built per
//! call and dies with the session, so there is no cross-call cache to invalidate as the
//! graph advances.
//!
//! ## Kind filtering and limit
//! `algo.pagerank` takes a trailing `result_label` and `limit`: the procedure filters the
//! scored nodes to the requested label and truncates to the top `k` by score before the rows
//! cross back. So the ranking is exactly the top-`k` nodes of one [`SearchKind`], best-first,
//! in a single bounded call — no full-graph transfer, no label scan, no Rust-side sort.

use std::time::Instant;

use selene_core::{NodeId, Value};

use crate::convert::string_value;
use crate::error::StoreError;
use crate::gql::BoundQuery;
use crate::search::{SearchHit, SearchKind, extract_hits, k_value};
use crate::store::Store;

/// The node labels the associative projection spans — the memory entities and the records
/// that reference them. Trusted static identifiers (a closed set).
const PROJECTION_NODE_LABELS: [&str; 3] = ["Entity", "Fact", "Episode"];
/// The edge types the associative projection spans: a fact or episode is associatively near
/// an entity it is about or mentions, and facts chain through support.
const PROJECTION_EDGE_TYPES: [&str; 3] = ["MENTIONS", "ABOUT", "SUPPORTS"];
/// The ephemeral projection's name. It lives only for the session that builds it, so a
/// fixed name is safe — there is never more than one at a time.
const PROJECTION_NAME: &str = "aionforge_assoc";

/// PageRank damping (the conventional 0.85): the teleport floor that keeps convergence.
const DAMPING: f64 = 0.85;
/// Iteration cap — PageRank converges well within this on a memory-sized graph.
const MAX_ITERATIONS: i64 = 30;
/// Convergence tolerance; iteration stops once the score delta falls below it.
const TOLERANCE: f64 = 1e-6;
/// The restart weight given each seed entity (uniform; selene normalizes the set).
const SEED_WEIGHT: f64 = 1.0;
/// PageRank traversal orientation: spread mass across each edge both ways so a seed entity
/// reaches its facts and episodes (see the module "Why undirected" note).
const ORIENTATION_UNDIRECTED: &str = "undirected";

impl Store {
    /// Personalized-PageRank a kind by associative proximity to `seeds` (M3.T01).
    ///
    /// `seeds` are the entity nodes a query mentions; the returned [`SearchHit`]s are the
    /// nodes carrying `kind`'s label — any [`SearchKind`] is accepted here, and retrieval
    /// (L1) gates which kinds it asks for — ranked best-first by PageRank score: higher is
    /// nearer, the opposite of the cosine signals, but rank fusion reads only position so
    /// the two never have to be made comparable. An empty `seeds` yields an empty ranking:
    /// a graph signal needs a personalization root, and a uniform PageRank is not the
    /// associative prior this signal is for.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a seed is not in the projection, or the projection build,
    /// the PageRank call, a bind, or execution fails.
    pub fn personalized_pagerank(
        &self,
        kind: SearchKind,
        seeds: &[NodeId],
        k: usize,
    ) -> Result<Vec<SearchHit>, StoreError> {
        self.personalized_pagerank_within(kind, seeds, k, None)
    }

    /// [`Store::personalized_pagerank`] bounded by an optional recall deadline.
    ///
    /// `None` is identical to [`Store::personalized_pagerank`]. A `Some(deadline)` lets
    /// the retriever abort the projection-build + PageRank `CALL`s mid-statement when
    /// the recall budget expires — the deadline rides the single shared session.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a seed is not in the projection, or the projection
    /// build, the PageRank call, a bind, execution, or the deadline fails.
    pub fn personalized_pagerank_within(
        &self,
        kind: SearchKind,
        seeds: &[NodeId],
        k: usize,
        deadline: Option<Instant>,
    ) -> Result<Vec<SearchHit>, StoreError> {
        if seeds.is_empty() || k == 0 {
            return Ok(Vec::new());
        }

        // Build the ephemeral associative projection and PageRank over it in one session
        // (the projection is session-scoped). `result_label` filters the scored nodes to the
        // requested kind and `limit` truncates to the top `k` by score inside the procedure;
        // the `WHERE score > 0.0` yield-filter then drops any node outside the seed's
        // connected component (zero personalized mass).
        let build = BoundQuery::new("CALL algo.projection_build($name, $labels, $types, NULL)")
            .bind_str("name", PROJECTION_NAME)?
            .bind("labels", string_list_value(&PROJECTION_NODE_LABELS)?)?
            .bind("types", string_list_value(&PROJECTION_EDGE_TYPES)?)?;
        let rank = BoundQuery::new(
            "CALL algo.pagerank($name, $damping, $max_iter, $tolerance, NULL, \
             $orientation, $seeds, $result_label, $limit) YIELD node_id, score \
             WHERE score > 0.0",
        )
        .bind_str("name", PROJECTION_NAME)?
        .bind("damping", Value::Float(DAMPING))?
        .bind("max_iter", Value::Int(MAX_ITERATIONS))?
        .bind("tolerance", Value::Float(TOLERANCE))?
        .bind_str("orientation", ORIENTATION_UNDIRECTED)?
        .bind("seeds", personalization_value(seeds))?
        .bind_str("result_label", kind.label())?
        .bind("limit", k_value(k))?;

        // PageRank already filtered to `kind` and returned the top `k` best-first, so the
        // yielded rows are the ranking — no Rust-side label intersection, sort, or truncate.
        extract_hits(
            self.execute_session_within(&[build, rank], deadline)?,
            "score",
        )
    }
}

/// The personalization seed list `algo.pagerank` expects: a list of `[node, weight]` pairs
/// (the 2-element-list seed form), each seed given the same restart weight.
fn personalization_value(seeds: &[NodeId]) -> Value {
    Value::List(
        seeds
            .iter()
            .copied()
            .map(|node| Value::List(vec![Value::NodeRef(node), Value::Float(SEED_WEIGHT)]))
            .collect(),
    )
}

/// A `LIST<STRING>` bound-parameter value from trusted static identifiers.
fn string_list_value(items: &[&str]) -> Result<Value, StoreError> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(string_value(item)?);
    }
    Ok(Value::List(out))
}
