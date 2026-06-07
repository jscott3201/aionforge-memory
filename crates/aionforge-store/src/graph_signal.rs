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
//! ## Kind filtering (interim)
//! `CALL algo.pagerank` yields `(node_id, score)` over every projected node with no label,
//! and a standalone `CALL ... YIELD` admits neither a label predicate nor `RETURN`/`ORDER
//! BY`/`LIMIT`, so the ranking is filtered to one [`SearchKind`] *here*: intersect the PPR
//! scores with the kind's node set, then sort and cap in Rust. The node set comes from a
//! label scan, which is the one inefficiency on this path — tracked for replacement by a
//! selene-side label + limit on `algo.pagerank` (mirroring `selene.vector_search_nodes`),
//! after which this becomes a single bounded call with no scan and no Rust sort.

use std::cmp::Ordering;
use std::collections::HashSet;

use selene_core::{NodeId, Value};

use crate::convert::{as_node_ref, string_value};
use crate::error::StoreError;
use crate::gql::{BoundQuery, QueryResult};
use crate::search::{SearchHit, SearchKind, extract_hits};
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
    /// `kind` nodes (facts or episodes) ranked best-first by PageRank score — higher is
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
        if seeds.is_empty() || k == 0 {
            return Ok(Vec::new());
        }

        // Build the ephemeral associative projection and PageRank over it in one session
        // (the projection is session-scoped). The `WHERE score > 0.0` yield-filter drops
        // nodes outside the seed's connected component before the rows cross back.
        let build = BoundQuery::new("CALL algo.projection_build($name, $labels, $types, NULL)")
            .bind_str("name", PROJECTION_NAME)?
            .bind("labels", string_list_value(&PROJECTION_NODE_LABELS)?)?
            .bind("types", string_list_value(&PROJECTION_EDGE_TYPES)?)?;
        let rank = BoundQuery::new(
            "CALL algo.pagerank($name, $damping, $max_iter, $tolerance, NULL, \
             $orientation, $seeds) YIELD node_id, score WHERE score > 0.0",
        )
        .bind_str("name", PROJECTION_NAME)?
        .bind("damping", Value::Float(DAMPING))?
        .bind("max_iter", Value::Int(MAX_ITERATIONS))?
        .bind("tolerance", Value::Float(TOLERANCE))?
        .bind_str("orientation", ORIENTATION_UNDIRECTED)?
        .bind("seeds", personalization_value(seeds))?;

        let ranked = extract_hits(self.execute_session(&[build, rank])?, "score")?;

        // Keep only nodes of `kind`, then sort best-first and cap at `k`. PageRank cannot
        // order or limit in the CALL, so both happen here.
        let of_kind = self.kind_node_set(kind)?;
        let mut hits: Vec<SearchHit> = ranked
            .into_iter()
            .filter(|hit| of_kind.contains(&hit.node))
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.node.cmp(&b.node))
        });
        hits.truncate(k);
        Ok(hits)
    }

    /// The set of node ids carrying `kind`'s label. A label scan — the interim cost the
    /// module note tracks for replacement by a selene-side label filter on `algo.pagerank`.
    fn kind_node_set(&self, kind: SearchKind) -> Result<HashSet<NodeId>, StoreError> {
        let label = kind.label();
        let source = format!("MATCH (n:{label}) RETURN n AS node_id"); // gql-ident-ok
        let QueryResult::Rows(rows) = self.execute(&BoundQuery::new(source))? else {
            return Ok(HashSet::new());
        };
        let Some(idx) = rows.column_index("node_id") else {
            return Ok(HashSet::new());
        };
        let mut set = HashSet::with_capacity(rows.row_count());
        for row in 0..rows.row_count() {
            let value = rows
                .value(row, idx)
                .ok_or_else(|| StoreError::decode("label-scan row missing node_id"))?;
            set.insert(as_node_ref(value)?);
        }
        Ok(set)
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
