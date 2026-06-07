//! The high-precision graph candidate seed (03 §4).
//!
//! The factual and temporal-current classes take a high-precision path: derive a narrow
//! graph candidate set from the query, compose it with the maintained
//! `current_support_facts` set via native candidate-set algebra, then exact-vector-rerank
//! the bounded result. This module owns step one — deriving the seed.
//!
//! The seed is a *precision* device, not associative expansion (graph expansion stays
//! suppressed for the factual class). In v1 it is one of:
//! - **Scope membership** — the `scope_membership` provider, when scopes are populated.
//!   This is the precise, organizationally-grounded seed; in early milestones scopes are
//!   usually empty, so it commonly yields nothing and the entity path takes over.
//! - **Query-mention entity roots** — the canonical entities the query names (resolved by
//!   vector search over the entity index), expanded to every fact about them via the
//!   scalar-indexed `Fact.subject_id` lookup. This grounds the seed in entity identity
//!   rather than embedding-space neighborhood.
//!
//! When neither yields anything (no scopes, no resolvable entity, or no query embedding)
//! the seed is `None` and the caller falls back to the plain current-set path — the seed
//! never silences a recall that would otherwise have surfaced current facts.

use aionforge_domain::embedding::Embedding;
use aionforge_store::{CandidateSet, NodeId, SearchKind, Store};

use crate::error::RetrievalError;

/// How many canonical entities a query is resolved to before expanding to their facts.
/// Small and bounded: a precision seed wants the few entities the query actually names,
/// not a broad neighborhood.
const ENTITY_ROOTS: usize = 5;

/// Derive the high-precision graph candidate seed for a query (03 §4 step 1).
///
/// Returns `Some(nodes)` — a de-duplicated, deterministically ordered list of `Fact`
/// node ids to compose with the current-support set — or `None` when no seed could be
/// derived (the caller then uses the plain current-set path).
///
/// # Errors
/// Returns [`RetrievalError`] if a provider read or a store search fails.
pub(crate) fn derive_graph_seed(
    store: &Store,
    embedding: Option<&Embedding>,
) -> Result<Option<Vec<NodeId>>, RetrievalError> {
    // Scope membership is the precise primary seed when scopes are populated.
    let scope = store.candidate_state_members(CandidateSet::ScopeMembership)?;
    if !scope.is_empty() {
        return Ok(Some(dedup_sorted(scope)));
    }

    // Otherwise fall back to the entities the query names. Without a query embedding the
    // entities cannot be resolved, so there is no seed and the caller uses the plain path.
    let Some(embedding) = embedding else {
        return Ok(None);
    };

    let roots = store.vector_search_ann(SearchKind::Entity, embedding, ENTITY_ROOTS)?;
    let mut facts: Vec<NodeId> = Vec::new();
    for hit in roots {
        let Some(entity) = store.entity_by_node_id(hit.node)? else {
            continue;
        };
        facts.extend(store.facts_by_subject(&entity.identity.id)?);
    }
    if facts.is_empty() {
        Ok(None)
    } else {
        Ok(Some(dedup_sorted(facts)))
    }
}

/// Sort and de-duplicate node ids so the seed is a stable set — the same entities always
/// produce the same candidate list, preserving recall determinism (03 §6). Order does not
/// affect the set-algebra composition, but a stable seed keeps the whole path reproducible.
fn dedup_sorted(mut nodes: Vec<NodeId>) -> Vec<NodeId> {
    nodes.sort_unstable();
    nodes.dedup();
    nodes
}
