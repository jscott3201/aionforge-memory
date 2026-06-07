//! Native search primitives over selene-db's `CALL` procedures (03 §1–§4).
//!
//! Retrieval (L1) composes these; this module is the only place the engine's
//! vector, BM25, and candidate-state-scoped search procedures are named. Every
//! primitive is a fixed-source, parameter-bound `CALL`: the searchable label and
//! property are trusted static identifiers drawn from a closed [`SearchKind`] set
//! (interpolated into the source and marked `// gql-ident-ok`, because GQL cannot
//! bind an identifier as a parameter), while every caller value — the query vector,
//! `k`, and any candidate node list — travels as a bound parameter, so the parsed
//! statement never depends on caller input.
//!
//! All vector indexes are cosine (data-model §7), so every vector call passes the
//! `cosine` metric explicitly — ranking does not depend on whether a stored vector
//! happens to be unit-normalized. Cosine distance is lower-is-better; BM25 score is
//! higher-is-better. Either way the returned list is ordered best-first, which is all
//! rank fusion (M1.T05) needs.

use aionforge_domain::Embedding;
use selene_core::{NodeId, Value};

use crate::convert::{as_f64, as_node_ref, embedding_value};
use crate::error::StoreError;
use crate::gql::{BoundQuery, QueryResult};
use crate::store::Store;

/// A node and the engine score that ranked it (a 03 §6 signal contribution).
///
/// `score` is cosine distance for vector hits (lower is nearer) and the BM25
/// relevance score for text hits (higher is better); the list it appears in is
/// ordered best-first regardless.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// The matched node.
    pub node: NodeId,
    /// The raw engine score that ranked it.
    pub score: f64,
}

/// A vector-searchable node kind and its indexed properties (data-model §7–§8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    /// Raw captured turns.
    Episode,
    /// Extracted facts.
    Fact,
    /// Canonical entities.
    Entity,
    /// Stored skills (keyed on the problem embedding).
    Skill,
    /// Derived notes.
    Note,
    /// Known bad patterns.
    BadPattern,
    /// Identity core blocks.
    CoreBlock,
}

impl SearchKind {
    /// The selene-db node label.
    pub(crate) fn label(self) -> &'static str {
        match self {
            SearchKind::Episode => "Episode",
            SearchKind::Fact => "Fact",
            SearchKind::Entity => "Entity",
            SearchKind::Skill => "Skill",
            SearchKind::Note => "Note",
            SearchKind::BadPattern => "BadPattern",
            SearchKind::CoreBlock => "CoreBlock",
        }
    }

    /// The versioned embedding property (a skill embeds its problem, not its body).
    fn vector_property(self) -> &'static str {
        match self {
            SearchKind::Skill => "problem_embedding_v1",
            _ => "embedding_v1",
        }
    }

    /// The BM25 text property, for the kinds that maintain a text index (§8).
    fn text_property(self) -> Option<&'static str> {
        match self {
            SearchKind::Episode => Some("content"),
            SearchKind::Fact => Some("statement"),
            SearchKind::Entity => Some("canonical_name"),
            SearchKind::Skill => Some("description"),
            SearchKind::Note => Some("content"),
            SearchKind::BadPattern | SearchKind::CoreBlock => None,
        }
    }
}

/// A maintained candidate-state set (data-model §9), referenced by stable name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateSet {
    /// Facts with no live supersession or contradiction.
    CurrentSupportFacts,
    /// [`CandidateSet::CurrentSupportFacts`] grounded by incoming support and provenance.
    ProvenanceCurrentSupportFacts,
    /// Anything with a live scope membership.
    ScopeMembership,
    /// Anything recently active.
    RecencyActive,
    /// Facts that nothing currently contradicts.
    UnresolvedCurrent,
}

impl CandidateSet {
    /// The stable provider name the engine knows this set by (data-model §9).
    pub(crate) fn as_name(self) -> &'static str {
        match self {
            CandidateSet::CurrentSupportFacts => "current_support_facts",
            CandidateSet::ProvenanceCurrentSupportFacts => "provenance_current_support_facts",
            CandidateSet::ScopeMembership => "scope_membership",
            CandidateSet::RecencyActive => "recency_active",
            CandidateSet::UnresolvedCurrent => "unresolved_current",
        }
    }
}

/// A native set-algebra operation over a maintained set and an explicit node list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    /// Members in both the maintained set and the candidate list.
    Intersection,
    /// Members in either.
    Union,
    /// Members in the maintained set but not the candidate list.
    StateDifference,
    /// Members in the candidate list but not the maintained set.
    CandidatesDifference,
}

impl SetOp {
    /// The operation name the engine procedure expects.
    fn as_name(self) -> &'static str {
        match self {
            SetOp::Intersection => "intersection",
            SetOp::Union => "union",
            SetOp::StateDifference => "state_difference",
            SetOp::CandidatesDifference => "candidates_difference",
        }
    }
}

/// A graph edge type a candidate expansion may walk (03 §1 support expansion). A closed,
/// trusted set, so the label is a safe static identifier in a `CALL` source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandEdge {
    /// `Fact|Episode -SUPPORTS-> Fact`: a fact's supporting evidence.
    Supports,
}

impl ExpandEdge {
    /// The selene-db edge label.
    fn as_label(self) -> &'static str {
        match self {
            ExpandEdge::Supports => "SUPPORTS",
        }
    }
}

/// Which way a candidate expansion walks an edge. For `SUPPORTS` (`evidence -> fact`),
/// `Incoming` gathers the evidence that supports the root facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandDirection {
    /// Follow edges pointing *into* the roots (a root fact's supporting evidence).
    Incoming,
    /// Follow edges pointing *out of* the roots.
    Outgoing,
    /// Follow edges in either direction.
    Both,
}

impl ExpandDirection {
    /// The direction name the engine procedure expects.
    fn as_name(self) -> &'static str {
        match self {
            ExpandDirection::Incoming => "incoming",
            ExpandDirection::Outgoing => "outgoing",
            ExpandDirection::Both => "both",
        }
    }
}

impl Store {
    /// Approximate nearest-neighbor vector search over the HNSW index (03 §1 dense).
    ///
    /// The fast retrieval path and the capture near-duplicate check; validate recall
    /// against [`Store::vector_search_exact`], the full-precision oracle.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the query vector, the bind, or execution fails.
    pub fn vector_search_ann(
        &self,
        kind: SearchKind,
        query: &Embedding,
        k: usize,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let (label, prop) = (kind.label(), kind.vector_property());
        let source = format!(
            "CALL selene.vector_search_nodes_ann('{label}', '{prop}', $query, $k, 'cosine') YIELD node_id, distance"
        ); // gql-ident-ok
        let query = BoundQuery::new(source)
            .bind("query", embedding_value(query)?)?
            .bind("k", k_value(k))?;
        extract_hits(self.execute(&query)?, "distance")
    }

    /// Exact, full-precision vector search — the oracle for the ANN path (03 §1).
    ///
    /// # Errors
    /// Returns [`StoreError`] if the query vector, the bind, or execution fails.
    pub fn vector_search_exact(
        &self,
        kind: SearchKind,
        query: &Embedding,
        k: usize,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let (label, prop) = (kind.label(), kind.vector_property());
        let source = format!(
            "CALL selene.vector_search_nodes('{label}', '{prop}', $query, $k, 'cosine') YIELD node_id, distance"
        ); // gql-ident-ok
        let query = BoundQuery::new(source)
            .bind("query", embedding_value(query)?)?
            .bind("k", k_value(k))?;
        extract_hits(self.execute(&query)?, "distance")
    }

    /// Exact rerank of a bounded candidate set over full-precision vectors (03 §4).
    ///
    /// # Errors
    /// Returns [`StoreError`] if the query vector, the binds, or execution fails.
    pub fn vector_rerank(
        &self,
        kind: SearchKind,
        query: &Embedding,
        candidates: &[NodeId],
        k: usize,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let prop = kind.vector_property();
        let source = format!(
            "CALL selene.vector_score_nodes('{prop}', $query, $nodes, $k, 'cosine') YIELD node_id, distance"
        ); // gql-ident-ok
        let query = BoundQuery::new(source)
            .bind("query", embedding_value(query)?)?
            .bind("nodes", node_list_value(candidates))?
            .bind("k", k_value(k))?;
        extract_hits(self.execute(&query)?, "distance")
    }

    /// Vector-score a maintained candidate-state set directly (03 §4 high precision).
    ///
    /// The set name resolves against the providers attached at construction; the
    /// `status = 'active'` half of `current_support_facts` (§9) is a query-time
    /// filter the caller layers on top, not part of this primitive.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the set is unknown, the provider is stale, or the
    /// query vector, bind, or execution fails.
    pub fn vector_score_state(
        &self,
        kind: SearchKind,
        query: &Embedding,
        set: CandidateSet,
        k: usize,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let prop = kind.vector_property();
        let name = set.as_name();
        let source = format!(
            "CALL selene.vector_score_candidate_state('{prop}', $query, '{name}', $k, 'cosine') YIELD node_id, distance"
        ); // gql-ident-ok
        let query = BoundQuery::new(source)
            .bind("query", embedding_value(query)?)?
            .bind("k", k_value(k))?;
        extract_hits(self.execute(&query)?, "distance")
    }

    /// Compose a maintained set with an explicit node list via native set algebra,
    /// then exact-vector-rerank the result (03 §2 candidate-set algebra, §4).
    ///
    /// This is the composed candidate producer the high-precision path uses — e.g.
    /// a scope-derived node list intersected with `current_support_facts` — kept
    /// inside the engine rather than re-implemented above it.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the set is unknown, the provider is stale, or the
    /// query vector, binds, or execution fails.
    pub fn vector_score_state_nodes(
        &self,
        kind: SearchKind,
        query: &Embedding,
        set: CandidateSet,
        candidates: &[NodeId],
        op: SetOp,
        k: usize,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let prop = kind.vector_property();
        let (name, op) = (set.as_name(), op.as_name());
        let source = format!(
            "CALL selene.vector_score_candidate_state_nodes('{prop}', $query, '{name}', $nodes, $k, '{op}', 'cosine') YIELD node_id, distance"
        ); // gql-ident-ok
        let query = BoundQuery::new(source)
            .bind("query", embedding_value(query)?)?
            .bind("nodes", node_list_value(candidates))?
            .bind("k", k_value(k))?;
        extract_hits(self.execute(&query)?, "distance")
    }

    /// Expand `roots` one hop over `edge`, compose with a maintained candidate-state set,
    /// then exact-vector-rerank the composed set (03 §1 support expansion, §4).
    ///
    /// The roots are preserved and graph-expanded one labelled hop in `direction`; the
    /// resulting set (`roots ∪ neighbors`) is composed with `set` via `op`, and the
    /// composition is scored by cosine distance to `query` over `kind`'s vector property —
    /// all under one statement snapshot. With `set = current_support_facts` and `op =
    /// Intersection` the scored set is current-scoped natively, so support-derived evidence
    /// facts a plain ANN pass misses surface while non-current facts are filtered out. Empty
    /// `roots` yields an empty ranking (no expansion root, no global scan).
    ///
    /// # Errors
    /// Returns [`StoreError`] if the set is unknown, the provider is stale, or the query
    /// vector, the binds, or execution fails.
    #[allow(clippy::too_many_arguments)]
    pub fn vector_score_state_expanded(
        &self,
        kind: SearchKind,
        query: &Embedding,
        set: CandidateSet,
        roots: &[NodeId],
        edge: ExpandEdge,
        direction: ExpandDirection,
        op: SetOp,
        k: usize,
    ) -> Result<Vec<SearchHit>, StoreError> {
        if roots.is_empty() {
            return Ok(Vec::new());
        }
        let prop = kind.vector_property();
        let (name, op, label, dir) = (
            set.as_name(),
            op.as_name(),
            edge.as_label(),
            direction.as_name(),
        );
        let source = format!(
            "CALL selene.vector_score_candidate_state_expanded('{prop}', $query, '{name}', $roots, '{label}', $k, '{op}', '{dir}', 'cosine') YIELD node_id, distance"
        ); // gql-ident-ok
        let query = BoundQuery::new(source)
            .bind("query", embedding_value(query)?)?
            .bind("roots", node_list_value(roots))?
            .bind("k", k_value(k))?;
        extract_hits(self.execute(&query)?, "distance")
    }

    /// Native BM25 search over the maintained text index (03 §1 lexical).
    ///
    /// # Errors
    /// Returns [`StoreError::Search`] if `kind` maintains no text index, or another
    /// [`StoreError`] if the bind or execution fails.
    pub fn text_search(
        &self,
        kind: SearchKind,
        query: &str,
        k: usize,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let label = kind.label();
        let prop = text_property(kind)?;
        let source = format!(
            "CALL selene.text_search_nodes('{label}', '{prop}', $query, $k) YIELD node_id, score"
        ); // gql-ident-ok
        let query = BoundQuery::new(source)
            .bind_str("query", query)?
            .bind("k", k_value(k))?;
        extract_hits(self.execute(&query)?, "score")
    }

    /// BM25-score an explicit candidate set over the text index (03 §2 scoped lexical).
    ///
    /// # Errors
    /// Returns [`StoreError::Search`] if `kind` maintains no text index, or another
    /// [`StoreError`] if the binds or execution fails.
    pub fn text_score_nodes(
        &self,
        kind: SearchKind,
        query: &str,
        candidates: &[NodeId],
        k: usize,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let label = kind.label();
        let prop = text_property(kind)?;
        let source = format!(
            "CALL selene.text_score_nodes('{label}', '{prop}', $query, $nodes, $k) YIELD node_id, score"
        ); // gql-ident-ok
        let query = BoundQuery::new(source)
            .bind_str("query", query)?
            .bind("nodes", node_list_value(candidates))?
            .bind("k", k_value(k))?;
        extract_hits(self.execute(&query)?, "score")
    }
}

/// The BM25 text property for `kind`, or a [`StoreError::Search`] if it has none.
fn text_property(kind: SearchKind) -> Result<&'static str, StoreError> {
    kind.text_property()
        .ok_or_else(|| StoreError::search(format!("{} maintains no text index", kind.label())))
}

/// A `k` limit as the engine integer the search procedures expect.
pub(crate) fn k_value(k: usize) -> Value {
    Value::Int(i64::try_from(k).unwrap_or(i64::MAX))
}

/// A candidate node list as a bound `LIST<NODE>` parameter value.
fn node_list_value(nodes: &[NodeId]) -> Value {
    Value::List(nodes.iter().copied().map(Value::NodeRef).collect())
}

/// Read `(node_id, <score_col>)` rows from a search result into best-first hits.
pub(crate) fn extract_hits(
    result: QueryResult,
    score_col: &str,
) -> Result<Vec<SearchHit>, StoreError> {
    let rows = match result {
        QueryResult::Rows(rows) => rows,
        // A `YIELD` always projects a (possibly empty) row table, so a search CALL
        // yields `Rows`; `Empty` would only come from a non-projecting statement.
        QueryResult::Empty => return Ok(Vec::new()),
        // These are read-only CALLs by construction. A `Written` result means the
        // statement modified the graph, which a search primitive must never do.
        QueryResult::Written { .. } => {
            return Err(StoreError::decode(
                "a search statement unexpectedly modified the graph",
            ));
        }
    };
    let node_idx = rows
        .column_index("node_id")
        .ok_or_else(|| StoreError::decode("search result has no node_id column"))?;
    let score_idx = rows
        .column_index(score_col)
        .ok_or_else(|| StoreError::decode(format!("search result has no {score_col} column")))?;
    let mut hits = Vec::with_capacity(rows.row_count());
    for row in 0..rows.row_count() {
        let node = as_node_ref(
            rows.value(row, node_idx)
                .ok_or_else(|| StoreError::decode("search row missing node_id"))?,
        )?;
        let score = as_f64(
            rows.value(row, score_idx)
                .ok_or_else(|| StoreError::decode("search row missing score"))?,
        )?;
        hits.push(SearchHit { node, score });
    }
    Ok(hits)
}
