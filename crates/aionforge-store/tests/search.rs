//! Integration tests for the L0 native-search primitives (03 §1–§4).
//!
//! These are hermetic: embeddings are small hand-built vectors and the corpus is
//! seeded in-process, so the suite needs no embedder and runs the same in CI as it
//! does locally. The dense path is validated against the engine's own exact
//! (full-precision) search as the oracle; the lexical path against known
//! term-membership; and the candidate-state path is smoke-tested for procedure and
//! provider-name resolution (its ranking behaviour over real `Fact` data lands with
//! the dense-retrieval task, M1.T04, where facts exist through the capture path).

use std::collections::HashSet;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::embedding::Embedding;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::time::Timestamp;

use aionforge_store::{CandidateSet, NodeId, SearchKind, SetOp, Store, StoreConfig, StoreError};

/// Parse a fixed zoned datetime so the tests are deterministic.
fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

/// A migrated, in-memory store whose vector indexes are pinned at dimension 4 to
/// match the toy embeddings below.
fn store() -> Store {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store
        .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
        .expect("migrate store");
    store
}

fn stats() -> Stats {
    Stats {
        importance: 0.5,
        trust: 0.8,
        last_access: ts("2026-06-06T10:00:00-05:00[America/Chicago]"),
        access_count_recent: 1,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

/// A minimal episode with the given content and optional embedding.
fn episode(content: &str, embedding: Option<Vec<f32>>) -> Episode {
    Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts("2026-06-06T09:30:00-05:00[America/Chicago]"),
            namespace: Namespace::Agent("alice".to_string()),
            expired_at: None,
        },
        stats: stats(),
        content: content.to_string(),
        role: Role::User,
        captured_at: ts("2026-06-06T09:29:59-05:00[America/Chicago]"),
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: embedding.map(|v| Embedding::new(v).expect("finite embedding")),
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    }
}

/// Seed an episode carrying an embedding; return its node id.
fn seed_vec(store: &Store, content: &str, embedding: Vec<f32>) -> NodeId {
    store
        .insert_episode(&episode(content, Some(embedding)))
        .expect("seed embedded episode")
}

/// Seed a text-only episode; return its node id.
fn seed_text(store: &Store, content: &str) -> NodeId {
    store
        .insert_episode(&episode(content, None))
        .expect("seed text episode")
}

fn emb(v: Vec<f32>) -> Embedding {
    Embedding::new(v).expect("finite embedding")
}

#[test]
fn vector_ann_matches_the_exact_oracle() {
    let store = store();
    let a = seed_vec(&store, "a", vec![1.0, 0.0, 0.0, 0.0]);
    let e = seed_vec(&store, "e", vec![0.9, 0.1, 0.0, 0.0]);
    seed_vec(&store, "b", vec![0.0, 1.0, 0.0, 0.0]);
    seed_vec(&store, "c", vec![0.0, 0.0, 1.0, 0.0]);
    seed_vec(&store, "d", vec![0.0, 0.0, 0.0, 1.0]);
    seed_vec(&store, "f", vec![0.1, 0.9, 0.0, 0.0]);

    let query = emb(vec![0.95, 0.05, 0.0, 0.0]);
    let exact = store
        .vector_search_exact(SearchKind::Episode, &query, 3)
        .expect("exact search");
    let ann = store
        .vector_search_ann(SearchKind::Episode, &query, 3)
        .expect("ann search");

    // The exact oracle ranks the two nearest, a then e, ahead of the rest.
    assert_eq!(exact.len(), 3);
    assert_eq!(exact[0].node, a, "nearest neighbour is wrong");
    assert_eq!(exact[1].node, e, "second neighbour is wrong");
    // Cosine distance is non-decreasing down a best-first list.
    assert!(
        exact.windows(2).all(|w| w[0].score <= w[1].score),
        "exact distances are not non-decreasing"
    );

    // On this corpus the HNSW index recovers the exact top-3 (recall = 1.0) and the
    // same nearest neighbour.
    let exact_nodes: HashSet<NodeId> = exact.iter().map(|h| h.node).collect();
    let ann_nodes: HashSet<NodeId> = ann.iter().map(|h| h.node).collect();
    assert_eq!(ann_nodes, exact_nodes, "ANN top-3 missed the exact oracle");
    assert_eq!(ann[0].node, a, "ANN nearest neighbour is wrong");
}

#[test]
fn a_past_deadline_aborts_a_vector_search_but_none_runs_to_completion() {
    use std::time::{Duration, Instant};

    let store = store();
    seed_vec(&store, "a", vec![1.0, 0.0, 0.0, 0.0]);
    seed_vec(&store, "b", vec![0.0, 1.0, 0.0, 0.0]);
    seed_vec(&store, "c", vec![0.0, 0.0, 1.0, 0.0]);
    let query = emb(vec![0.95, 0.05, 0.0, 0.0]);

    // No deadline: the search runs to completion, exactly as the plain entry point.
    let ok = store
        .vector_search_ann_within(SearchKind::Episode, &query, 3, None)
        .expect("a search with no deadline succeeds");
    assert!(!ok.is_empty(), "the no-deadline path returns hits");

    // A deadline already in the past: the engine aborts the CALL at its first
    // cooperative checkpoint rather than running it to completion.
    let past = Instant::now()
        .checked_sub(Duration::from_secs(60))
        .expect("an instant 60s in the past");
    let aborted = store.vector_search_ann_within(SearchKind::Episode, &query, 3, Some(past));
    assert!(
        aborted.is_err(),
        "a past deadline must abort the search, got {aborted:?}"
    );
}

#[test]
fn vector_rerank_orders_and_restricts_to_candidates() {
    let store = store();
    let a = seed_vec(&store, "a", vec![1.0, 0.0, 0.0, 0.0]);
    let e = seed_vec(&store, "e", vec![0.9, 0.1, 0.0, 0.0]);
    let b = seed_vec(&store, "b", vec![0.0, 1.0, 0.0, 0.0]);
    let c = seed_vec(&store, "c", vec![0.0, 0.0, 1.0, 0.0]);

    let query = emb(vec![0.95, 0.05, 0.0, 0.0]);
    let hits = store
        .vector_rerank(SearchKind::Episode, &query, &[a, e, b], 10)
        .expect("rerank");

    let nodes: Vec<NodeId> = hits.iter().map(|h| h.node).collect();
    // Exact rerank orders the candidates by cosine distance: a, then e, then b.
    assert_eq!(nodes, vec![a, e, b], "rerank order is wrong");
    // A node outside the candidate set never appears.
    assert!(!nodes.contains(&c), "rerank returned a non-candidate");
}

#[test]
fn bm25_returns_only_matching_documents() {
    let store = store();
    let g1 = seed_text(&store, "graph retrieval over memory");
    let v = seed_text(&store, "vector search and ranking");
    let g2 = seed_text(&store, "graph algorithms and traversal");

    let hits = store
        .text_search(SearchKind::Episode, "graph", 10)
        .expect("bm25 search");
    let nodes: HashSet<NodeId> = hits.iter().map(|h| h.node).collect();

    assert!(nodes.contains(&g1), "matching doc g1 missing");
    assert!(nodes.contains(&g2), "matching doc g2 missing");
    assert!(!nodes.contains(&v), "doc without the term should not match");
    assert!(
        hits.iter().all(|h| h.score > 0.0),
        "BM25 score should be positive"
    );
}

#[test]
fn bm25_candidate_scoped_restricts_to_candidates() {
    let store = store();
    let g1 = seed_text(&store, "graph retrieval over memory");
    let v = seed_text(&store, "vector search and ranking");
    seed_text(&store, "graph algorithms and traversal");

    let hits = store
        .text_score_nodes(SearchKind::Episode, "graph", &[g1, v], 10)
        .expect("scoped bm25");
    let nodes: Vec<NodeId> = hits.iter().map(|h| h.node).collect();

    // Of the two candidates, only g1 contains the term; the non-candidate g2 that
    // also matches is excluded because it was not in the candidate set.
    assert_eq!(nodes, vec![g1], "only the matching candidate should score");
}

#[test]
fn text_search_on_a_kind_without_an_index_errors() {
    let store = store();
    let err = store
        .text_search(SearchKind::BadPattern, "anything", 5)
        .expect_err("a kind with no text index must error");
    assert!(
        matches!(err, StoreError::Search(_)),
        "expected a Search error, got {err:?}"
    );
}

#[test]
fn candidate_state_primitives_resolve_against_the_providers() {
    let store = store();
    let query = emb(vec![1.0, 0.0, 0.0, 0.0]);

    // No facts exist yet, so every maintained set is empty — but the procedure and
    // the provider set name must resolve. A wrong procedure name, argument order, or
    // unknown set would surface as an error here rather than an empty result.
    let direct = store
        .vector_score_state(
            SearchKind::Fact,
            &query,
            CandidateSet::CurrentSupportFacts,
            5,
        )
        .expect("maintained-set vector score resolves");
    assert!(direct.is_empty(), "no facts means an empty set");

    let composed = store
        .vector_score_state_nodes(
            SearchKind::Fact,
            &query,
            CandidateSet::CurrentSupportFacts,
            &[],
            SetOp::Intersection,
            5,
        )
        .expect("maintained-set composition resolves");
    assert!(composed.is_empty(), "empty composition yields no hits");
}

#[test]
fn k_zero_returns_no_hits() {
    let store = store();
    let node = seed_vec(&store, "a", vec![1.0, 0.0, 0.0, 0.0]);
    seed_text(&store, "graph retrieval over memory");
    let query = emb(vec![1.0, 0.0, 0.0, 0.0]);

    // A zero result limit is a valid non-negative cardinality and asks for nothing.
    assert!(
        store
            .vector_search_ann(SearchKind::Episode, &query, 0)
            .expect("ann k=0")
            .is_empty()
    );
    assert!(
        store
            .vector_search_exact(SearchKind::Episode, &query, 0)
            .expect("exact k=0")
            .is_empty()
    );
    assert!(
        store
            .vector_rerank(SearchKind::Episode, &query, &[node], 0)
            .expect("rerank k=0")
            .is_empty()
    );
    assert!(
        store
            .text_search(SearchKind::Episode, "graph", 0)
            .expect("text k=0")
            .is_empty()
    );
}

#[test]
fn empty_candidate_set_returns_no_hits() {
    let store = store();
    // Seed a matching node that is deliberately left out of the candidate lists.
    seed_vec(&store, "a", vec![1.0, 0.0, 0.0, 0.0]);
    seed_text(&store, "graph retrieval over memory");
    let query = emb(vec![1.0, 0.0, 0.0, 0.0]);

    // An empty candidate set scopes the search to nothing, so nothing scores — even
    // though a match exists in the graph.
    assert!(
        store
            .vector_rerank(SearchKind::Episode, &query, &[], 5)
            .expect("rerank over empty candidates")
            .is_empty()
    );
    assert!(
        store
            .text_score_nodes(SearchKind::Episode, "graph", &[], 5)
            .expect("text score over empty candidates")
            .is_empty()
    );
}

#[test]
fn nearest_active_episode_returns_the_closest_active_match() {
    let store = store();
    // An expired (soft-forgotten) episode and an active one point the same way.
    let mut expired = episode("forgotten turn", Some(vec![1.0, 0.0, 0.0, 0.0]));
    expired.identity.expired_at = Some(ts("2026-06-07T00:00:00-05:00[America/Chicago]"));
    store
        .insert_episode(&expired)
        .expect("seed expired episode");

    let active = episode("remembered turn", Some(vec![1.0, 0.0, 0.0, 0.0]));
    let active_id = active.identity.id;
    store.insert_episode(&active).expect("seed active episode");

    let (id, distance) = store
        .nearest_active_episode(&emb(vec![1.0, 0.0, 0.0, 0.0]), 8)
        .expect("nearest active episode")
        .expect("an active neighbour exists");
    assert_eq!(id, active_id, "the soft-forgotten episode must be skipped");
    assert!(
        distance <= 0.001,
        "identical direction is ~0 distance, got {distance}"
    );
}

#[test]
fn nearest_active_episode_is_none_when_every_neighbour_is_expired() {
    let store = store();
    let mut expired = episode("forgotten turn", Some(vec![1.0, 0.0, 0.0, 0.0]));
    expired.identity.expired_at = Some(ts("2026-06-07T00:00:00-05:00[America/Chicago]"));
    store
        .insert_episode(&expired)
        .expect("seed expired episode");

    let nearest = store
        .nearest_active_episode(&emb(vec![1.0, 0.0, 0.0, 0.0]), 8)
        .expect("nearest active episode");
    assert!(
        nearest.is_none(),
        "an expired-only neighbourhood yields no active match"
    );
}
