//! Native PageRank as an associative retrieval signal (03 §1 graph, M3.T01) — both the
//! query-seeded and the seedless-global passes over the same associative projection.
//!
//! [`Store::personalized_pagerank_within`] seeds PageRank on the entities a query mentions
//! and reads back the facts and episodes that sit closest to them in the associative graph —
//! `Episode -MENTIONS-> Entity`, `Fact -ABOUT-> Entity`, and `Fact|Episode -SUPPORTS-> Fact`.
//! Retrieval (L1) gates this to the classes that benefit (multi-hop, entity) and fuses the
//! per-kind ranking with the others. [`Store::graph_authority`] runs the same projection
//! *seedless* (uniform teleport) for a query-independent global authority prior; the two share
//! one private `pagerank_within` core and differ only in the bound personalization.
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
//! the PageRank over it must run in a single session — hence [`Store::execute_session_within`]
//! rather than two [`Store::execute`] calls. The projection is ephemeral: it is built per
//! call and dies with the session, so there is no cross-call cache to invalidate as the
//! graph advances.
//!
//! ## Kind filtering, namespace scoping, and limit
//! `algo.pagerank` takes trailing `result_label`, `limit`, and `result_nodes` arguments
//! (selene-db 1.3): the procedure filters the scored nodes to the requested label, optionally
//! intersects them with an explicit `result_nodes` set, then truncates to the top `k` by score
//! — all before the rows cross back. The intersection runs before the truncation, so a
//! `result_nodes` scope yields the top-`k` of the in-scope ranking, not the in-scope members of
//! an already-truncated top-`k`. That ordering is what lets the retriever spend the graph
//! fan-out on the reader's visible-namespace episodes rather than a cross-namespace top-`k` a
//! post-fusion filter would mostly discard (03 §6 namespace scoping). So the ranking is exactly
//! the top-`k` nodes of one [`SearchKind`] within an optional node scope, best-first, in a
//! single bounded call — no full-graph transfer, no label scan, no Rust-side sort.

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
        self.personalized_pagerank_within(kind, seeds, k, None, None)
    }

    /// [`Store::personalized_pagerank`] scoped to an optional node set and bounded by an
    /// optional recall deadline.
    ///
    /// `result_nodes` is `Some(scope)` to restrict the ranking to an explicit node set — the
    /// reader's visible-namespace records of `kind` (03 §6 namespace scoping) — or `None` to
    /// rank over every projection node (the unscoped fact reach). The scope is intersected
    /// inside `algo.pagerank` *before* the top-`k` truncation, so the fan-out is spent on
    /// in-scope nodes rather than a cross-namespace top-`k`. `Some(empty)` is the
    /// reader-has-no-visible-record case: it short-circuits to an empty ranking rather than
    /// building a projection only to intersect to nothing — distinct from `None`.
    ///
    /// A `Some(deadline)` lets the retriever abort the projection-build + PageRank `CALL`s
    /// mid-statement when the recall budget expires — the deadline rides the single shared
    /// session; `None` runs unbounded.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a seed is not in the projection, or the projection
    /// build, the PageRank call, a bind, execution, or the deadline fails.
    pub fn personalized_pagerank_within(
        &self,
        kind: SearchKind,
        seeds: &[NodeId],
        k: usize,
        result_nodes: Option<&[NodeId]>,
        deadline: Option<Instant>,
    ) -> Result<Vec<SearchHit>, StoreError> {
        if seeds.is_empty() {
            // A graph signal needs a personalization root; no seeds means no associative prior,
            // not a uniform PageRank fallback (that is [`Store::graph_authority`]'s job).
            return Ok(Vec::new());
        }
        self.pagerank_within(
            kind,
            personalization_value(seeds),
            k,
            result_nodes,
            deadline,
        )
    }

    /// Globally rank a kind by undirected PageRank *authority* — a seedless, query-independent
    /// structural prior (R1, the global-authority fusion signal).
    ///
    /// Where [`Store::personalized_pagerank_within`] restarts mass on a query's entities (so its
    /// ranking answers "what is near *these* nodes"), this teleports uniformly: every projection
    /// node gets a standing score for how well-connected it is in the *whole* associative graph.
    /// It is the topological complement to the non-structural decayed-importance signal — a
    /// memory wired into many supported facts and mentioned entities carries more standing
    /// authority than an isolated one, regardless of the query.
    ///
    /// Seedless is the only difference from the personalized path: it binds `personalization`
    /// (`$seeds`) to `NULL`, which `algo.pagerank` resolves to uniform teleport (classic global
    /// PageRank). The `undirected` orientation is load-bearing here for the same reason as the
    /// personalized signal — the schema points records *at* entities, so a natural-orientation
    /// global PageRank would pool all authority at the entity sinks and starve the facts and
    /// episodes this ranking returns. `result_label`/`limit`/`result_nodes` push the kind filter,
    /// top-`k`, and visible-namespace scope into the procedure exactly as the personalized path
    /// does, so a global authority pass is still one bounded call, not a full-graph transfer.
    ///
    /// Unlike the personalized signal, an empty result is not the seedless case (there are no
    /// seeds) — it is the empty-store / `k == 0` / empty-scope case. Because uniform teleport
    /// gives every projection node a positive floor, the `score > 0.0` cut in the shared core is
    /// effectively a no-op here (no node has zero global mass); it stays for parity with the
    /// personalized path, where it drops out-of-component nodes.
    ///
    /// This recomputes the identical ranking on every recall (the input is the whole graph, not
    /// the query), so it pairs with a generation-keyed result cache — a separate store-side
    /// follow-up — to avoid repeating the pass; without the cache it costs one `algo.pagerank`
    /// per recall, the same order as the existing graph signal.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the projection build, the PageRank call, a bind, execution, or
    /// the deadline fails.
    pub fn graph_authority(
        &self,
        kind: SearchKind,
        k: usize,
        result_nodes: Option<&[NodeId]>,
        deadline: Option<Instant>,
    ) -> Result<Vec<SearchHit>, StoreError> {
        // NULL personalization => uniform teleport => classic global (seedless) PageRank, per the
        // `algo.pagerank` contract (`personalization` is nullable, default "NULL (uniform
        // teleport)"). Everything else is the shared associative-projection PageRank core.
        self.pagerank_within(kind, Value::Null, k, result_nodes, deadline)
    }

    /// The shared associative-projection PageRank core behind the personalized graph signal and
    /// the global authority prior. `personalization` is the bound `$seeds` value — a `[node,
    /// weight]` list for the personalized path, or `NULL` for the seedless/global path — and is
    /// the *only* axis the two callers differ on.
    fn pagerank_within(
        &self,
        kind: SearchKind,
        personalization: Value,
        k: usize,
        result_nodes: Option<&[NodeId]>,
        deadline: Option<Instant>,
    ) -> Result<Vec<SearchHit>, StoreError> {
        if k == 0 {
            return Ok(Vec::new());
        }
        // An explicit-but-empty scope means "no node of this kind is in scope" (the reader has
        // no visible record), which `algo.pagerank` would intersect to an empty result anyway.
        // Short-circuit so a no-scope reader never pays for the projection build. This is the
        // `Some(empty)` case only — `None` (unscoped) still ranks the whole projection.
        if matches!(result_nodes, Some(scope) if scope.is_empty()) {
            return Ok(Vec::new());
        }

        // Build the ephemeral associative projection and PageRank over it in one session
        // (the projection is session-scoped). `result_label` filters the scored nodes to the
        // requested kind, `result_nodes` (when bound non-NULL) intersects them with the
        // visible-namespace scope, and `limit` truncates to the top `k` by score inside the
        // procedure (intersection before truncation); the `score > 0.0` cut below then drops
        // any node with zero mass (outside a seed's connected component, in the personalized
        // case — a no-op under uniform teleport).
        let build = BoundQuery::new("CALL algo.projection_build($name, $labels, $types, NULL)")
            .bind_str("name", PROJECTION_NAME)?
            .bind("labels", string_list_value(&PROJECTION_NODE_LABELS)?)?
            .bind("types", string_list_value(&PROJECTION_EDGE_TYPES)?)?;
        let rank = BoundQuery::new(
            "CALL algo.pagerank($name, $damping, $max_iter, $tolerance, NULL, \
             $orientation, $seeds, $result_label, $limit, $result_nodes) YIELD node_id, score",
        )
        .bind_str("name", PROJECTION_NAME)?
        .bind("damping", Value::Float(DAMPING))?
        .bind("max_iter", Value::Int(MAX_ITERATIONS))?
        .bind("tolerance", Value::Float(TOLERANCE))?
        .bind_str("orientation", ORIENTATION_UNDIRECTED)?
        .bind("seeds", personalization)?
        .bind_str("result_label", kind.label())?
        .bind("limit", k_value(k))?
        .bind("result_nodes", result_nodes_value(result_nodes))?;

        // PageRank filtered to `kind` and returned the top `k` best-first. Drop any node with
        // zero mass: selene-db 1.3 removed the inline `CALL ... YIELD ... WHERE` shortcut, so
        // this `score > 0.0` cut is applied here instead of in the query. Rows arrive best-first
        // and a zero-mass node can only sort last, so the retained prefix is still the ranking —
        // a filter, not a re-sort.
        let mut hits = extract_hits(
            self.execute_session_within(&[build, rank], deadline)?,
            "score",
        )?;
        hits.retain(|hit| hit.score > 0.0);
        Ok(hits)
    }
}

/// The `result_nodes` argument `algo.pagerank` restricts its scored rows to: a `LIST<NODE>`
/// the procedure intersects with the kind-filtered ranking *before* truncating to `limit`, so
/// the yield is the top-`k` of the in-scope nodes. `None` binds `NULL` — the unscoped ranking
/// over every projection node of the kind.
fn result_nodes_value(nodes: Option<&[NodeId]>) -> Value {
    match nodes {
        Some(nodes) => Value::List(nodes.iter().copied().map(Value::NodeRef).collect()),
        None => Value::Null,
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
