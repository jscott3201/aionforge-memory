//! Graph-expanded support scoring (M3.T02) — L0 acceptance.
//!
//! Builds a tiny evidence graph — a root `Fact` near the query with a far-embedded
//! evidence `Fact` that `SUPPORTS` it — and scores it with [`Store::vector_score_state_expanded`].
//! The evidence fact is far from the query in vector space, so a plain
//! [`Store::vector_score_state_nodes`] over the root alone never reaches it; expanding the
//! root one incoming `SUPPORTS` hop and composing with `current_support_facts` recovers it.
//! These tests pin: incoming expansion recovers supporting evidence a plain pass misses;
//! the roots are preserved; the current-state composition filters a contradicted-but-active
//! evidence fact out; direction is load-bearing; empty roots and root-only graphs are safe;
//! and the metric is cosine.

mod common;

use aionforge_domain::embedding::Embedding;
use aionforge_domain::ids::Id;
use aionforge_domain::nodes::semantic::Entity;
use aionforge_domain::value::ObjectValue;

use aionforge_store::{
    BoundQuery, CandidateSet, ExpandDirection, ExpandEdge, NodeId, SearchKind, SetOp, Store, Value,
};

use common::{entity, fact, open_window, store, zdt};

const NEAR: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
const FAR: [f32; 4] = [0.0, 1.0, 0.0, 0.0];
const WINDOW_FROM: &str = "2026-06-06T09:30:00-05:00[America/Chicago]";

/// Insert an entity and return it with its node id (its embedding is irrelevant — the
/// expansion scores facts, and roots are passed by node id, not resolved by vector).
fn topic(store: &Store, name: &str) -> (Entity, NodeId) {
    let ent = entity(name);
    let node = store.insert_entity(&ent).expect("insert entity");
    (ent, node)
}

/// Assert a fact about `subject` carrying `embedding`, returning its `(domain id, node id)`.
fn embedded_fact(
    store: &Store,
    subject: &Entity,
    subject_node: NodeId,
    statement: &str,
    embedding: [f32; 4],
) -> (Id, NodeId) {
    let mut f = fact(
        subject.identity.id,
        "rel",
        ObjectValue::Text(statement.to_string()),
        statement,
    );
    f.embedding = Some(Embedding::new(embedding.to_vec()).expect("valid embedding"));
    let node = f.identity.id;
    let inserted = store
        .assert_fact(&f, subject_node, &open_window(WINDOW_FROM))
        .expect("assert fact");
    (node, inserted)
}

/// Wire `Fact -SUPPORTS-> Fact` by domain id (`weight` is `NOT NULL`, bound as a parameter).
fn support(store: &Store, from: &Id, to: &Id) {
    let q = BoundQuery::new(
        "MATCH (a:Fact {id: $from}), (b:Fact {id: $to}) \
         INSERT (a)-[:SUPPORTS {weight: $weight}]->(b)",
    )
    .bind_uuid("from", from)
    .unwrap()
    .bind_uuid("to", to)
    .unwrap()
    .bind("weight", Value::Float(1.0))
    .unwrap();
    store.execute(&q).expect("insert SUPPORTS");
}

/// Wire `Fact -CONTRADICTS-> Fact` by domain id. The outgoing source is quarantined out of
/// `current_support_facts` while its node `status` stays `Active`.
fn contradict(store: &Store, from: &Id, to: &Id) {
    let q = BoundQuery::new(
        "MATCH (a:Fact {id: $from}), (b:Fact {id: $to}) \
         INSERT (a)-[:CONTRADICTS {valid_from: $ts, ingested_at: $ts, detected_by: $by}]->(b)",
    )
    .bind_uuid("from", from)
    .unwrap()
    .bind_uuid("to", to)
    .unwrap()
    .bind("ts", zdt())
    .unwrap()
    .bind_str("by", "contradiction-detector")
    .unwrap();
    store.execute(&q).expect("insert CONTRADICTS");
}

fn query(vec: [f32; 4]) -> Embedding {
    Embedding::new(vec.to_vec()).expect("valid query embedding")
}

fn nodes_of(hits: &[aionforge_store::SearchHit]) -> Vec<NodeId> {
    hits.iter().map(|hit| hit.node).collect()
}

#[test]
fn incoming_expansion_recovers_supporting_evidence_a_plain_pass_misses() {
    let store = store();
    let (topic_ent, topic_node) = topic(&store, "topic");
    let (root_id, root_node) =
        embedded_fact(&store, &topic_ent, topic_node, "the root claim", NEAR);
    // Far-embedded evidence that supports the root: evidence -SUPPORTS-> root.
    let (ev_id, ev_node) = embedded_fact(
        &store,
        &topic_ent,
        topic_node,
        "far supporting evidence",
        FAR,
    );
    support(&store, &ev_id, &root_id);

    // A plain current-scoped score over the root alone never reaches the far evidence.
    let plain = store
        .vector_score_state_nodes(
            SearchKind::Fact,
            &query(NEAR),
            CandidateSet::CurrentSupportFacts,
            &[root_node],
            SetOp::Intersection,
            10,
        )
        .expect("plain score");
    assert!(
        !nodes_of(&plain).contains(&ev_node),
        "a plain pass over the root does not reach the far evidence",
    );

    // Expanding the root one incoming SUPPORTS hop recovers it, composed with the current set.
    let expanded = store
        .vector_score_state_expanded(
            SearchKind::Fact,
            &query(NEAR),
            CandidateSet::CurrentSupportFacts,
            &[root_node],
            ExpandEdge::Supports,
            ExpandDirection::Incoming,
            SetOp::Intersection,
            10,
        )
        .expect("expanded score");
    assert!(
        nodes_of(&expanded).contains(&ev_node),
        "incoming support expansion recovers the evidence fact: {expanded:?}",
    );
}

#[test]
fn expansion_preserves_the_roots() {
    let store = store();
    let (topic_ent, topic_node) = topic(&store, "topic");
    let (root_id, root_node) =
        embedded_fact(&store, &topic_ent, topic_node, "the root claim", NEAR);
    let (ev_id, _) = embedded_fact(&store, &topic_ent, topic_node, "evidence", FAR);
    support(&store, &ev_id, &root_id);

    let hits = store
        .vector_score_state_expanded(
            SearchKind::Fact,
            &query(NEAR),
            CandidateSet::CurrentSupportFacts,
            &[root_node],
            ExpandEdge::Supports,
            ExpandDirection::Incoming,
            SetOp::Intersection,
            10,
        )
        .expect("expanded score");
    assert!(
        nodes_of(&hits).contains(&root_node),
        "the root fact is preserved in the expanded scoring",
    );
}

#[test]
fn current_composition_excludes_a_contradicted_but_active_evidence_fact() {
    let store = store();
    let (topic_ent, topic_node) = topic(&store, "topic");
    let (root_id, root_node) =
        embedded_fact(&store, &topic_ent, topic_node, "the root claim", NEAR);
    let (ev_id, ev_node) = embedded_fact(&store, &topic_ent, topic_node, "contested evidence", FAR);
    support(&store, &ev_id, &root_id);
    // The evidence contradicts the root: its outgoing CONTRADICTS quarantines it out of
    // current_support_facts, though its status stays Active.
    contradict(&store, &ev_id, &root_id);

    let hits = store
        .vector_score_state_expanded(
            SearchKind::Fact,
            &query(NEAR),
            CandidateSet::CurrentSupportFacts,
            &[root_node],
            ExpandEdge::Supports,
            ExpandDirection::Incoming,
            SetOp::Intersection,
            10,
        )
        .expect("expanded score");
    assert!(
        !nodes_of(&hits).contains(&ev_node),
        "the current-state intersection filters the contradicted evidence out: {hits:?}",
    );
    assert!(
        nodes_of(&hits).contains(&root_node),
        "the still-current root remains",
    );

    // The same expansion with Union (state ∪ expanded) keeps the contradicted evidence —
    // it is in the expanded set, just not in the current-state set. This proves the
    // expansion did reach it, so the Intersection above excluded it via the current-state
    // composition, not because expansion silently failed to find it.
    let unioned = store
        .vector_score_state_expanded(
            SearchKind::Fact,
            &query(NEAR),
            CandidateSet::CurrentSupportFacts,
            &[root_node],
            ExpandEdge::Supports,
            ExpandDirection::Incoming,
            SetOp::Union,
            10,
        )
        .expect("unioned score");
    assert!(
        nodes_of(&unioned).contains(&ev_node),
        "Union keeps the expansion-reached evidence the Intersection dropped: {unioned:?}",
    );
}

#[test]
fn outgoing_direction_does_not_pull_incoming_evidence() {
    let store = store();
    let (topic_ent, topic_node) = topic(&store, "topic");
    let (root_id, root_node) =
        embedded_fact(&store, &topic_ent, topic_node, "the root claim", NEAR);
    let (ev_id, ev_node) = embedded_fact(&store, &topic_ent, topic_node, "evidence", FAR);
    support(&store, &ev_id, &root_id);

    // The evidence is *incoming* to the root; expanding outgoing finds nothing the root
    // supports, so the evidence is not recovered — direction is the load-bearing choice.
    let hits = store
        .vector_score_state_expanded(
            SearchKind::Fact,
            &query(NEAR),
            CandidateSet::CurrentSupportFacts,
            &[root_node],
            ExpandEdge::Supports,
            ExpandDirection::Outgoing,
            SetOp::Intersection,
            10,
        )
        .expect("expanded score");
    assert!(
        !nodes_of(&hits).contains(&ev_node),
        "outgoing expansion does not reach incoming evidence: {hits:?}",
    );
}

#[test]
fn empty_roots_yield_an_empty_ranking() {
    let store = store();
    let (topic_ent, topic_node) = topic(&store, "topic");
    embedded_fact(&store, &topic_ent, topic_node, "a fact", NEAR);

    assert!(
        store
            .vector_score_state_expanded(
                SearchKind::Fact,
                &query(NEAR),
                CandidateSet::CurrentSupportFacts,
                &[],
                ExpandEdge::Supports,
                ExpandDirection::Incoming,
                SetOp::Intersection,
                10,
            )
            .expect("empty roots are not an error")
            .is_empty(),
        "empty roots yield an empty ranking, never a global scan",
    );
}

#[test]
fn a_root_with_no_supports_edges_yields_only_the_root() {
    let store = store();
    let (topic_ent, topic_node) = topic(&store, "topic");
    let (_, root_node) = embedded_fact(&store, &topic_ent, topic_node, "the lone root", NEAR);

    let hits = store
        .vector_score_state_expanded(
            SearchKind::Fact,
            &query(NEAR),
            CandidateSet::CurrentSupportFacts,
            &[root_node],
            ExpandEdge::Supports,
            ExpandDirection::Incoming,
            SetOp::Intersection,
            10,
        )
        .expect("expanded score");
    assert_eq!(
        nodes_of(&hits),
        vec![root_node],
        "a root with no incoming support expands to just itself, never an error",
    );
}

#[test]
fn scoring_uses_cosine_distance_not_squared_euclidean() {
    let store = store();
    let (topic_ent, topic_node) = topic(&store, "topic");
    let (root_id, root_node) =
        embedded_fact(&store, &topic_ent, topic_node, "the root claim", NEAR);
    // Cosine-nearest by *direction* but far by magnitude: cosine distance ~0, squared
    // euclidean distance ~1. Under cosine it ranks above `off`; under squared euclidean it
    // would not — so the order pins the metric.
    let (aligned_id, aligned_node) = embedded_fact(
        &store,
        &topic_ent,
        topic_node,
        "aligned evidence",
        [2.0, 0.0, 0.0, 0.0],
    );
    let (off_id, off_node) = embedded_fact(
        &store,
        &topic_ent,
        topic_node,
        "off-axis evidence",
        [0.9, 0.1, 0.0, 0.0],
    );
    support(&store, &aligned_id, &root_id);
    support(&store, &off_id, &root_id);

    let hits = store
        .vector_score_state_expanded(
            SearchKind::Fact,
            &query(NEAR),
            CandidateSet::CurrentSupportFacts,
            &[root_node],
            ExpandEdge::Supports,
            ExpandDirection::Incoming,
            SetOp::Intersection,
            10,
        )
        .expect("expanded score");
    let order = nodes_of(&hits);
    let pos = |n: NodeId| order.iter().position(|m| *m == n);
    assert!(
        pos(aligned_node) < pos(off_node),
        "the direction-aligned evidence ranks ahead of the off-axis one under cosine: {hits:?}",
    );
}

#[test]
fn both_direction_reaches_neighbors_on_either_side() {
    let store = store();
    let (topic_ent, topic_node) = topic(&store, "topic");
    let (root_id, root_node) =
        embedded_fact(&store, &topic_ent, topic_node, "the root claim", NEAR);
    // Evidence supports the root (incoming); the root supports a downstream fact (outgoing).
    let (ev_id, ev_node) = embedded_fact(&store, &topic_ent, topic_node, "incoming evidence", FAR);
    support(&store, &ev_id, &root_id);
    let (down_id, down_node) =
        embedded_fact(&store, &topic_ent, topic_node, "downstream fact", FAR);
    support(&store, &root_id, &down_id);

    let hits = store
        .vector_score_state_expanded(
            SearchKind::Fact,
            &query(NEAR),
            CandidateSet::CurrentSupportFacts,
            &[root_node],
            ExpandEdge::Supports,
            ExpandDirection::Both,
            SetOp::Intersection,
            10,
        )
        .expect("expanded score");
    let got = nodes_of(&hits);
    assert!(
        got.contains(&ev_node) && got.contains(&down_node),
        "Both reaches the incoming evidence and the outgoing downstream fact: {hits:?}",
    );
}

#[test]
fn multiple_roots_expand_independently() {
    let store = store();
    let (topic_ent, topic_node) = topic(&store, "topic");
    let (r1_id, r1_node) = embedded_fact(&store, &topic_ent, topic_node, "first root", NEAR);
    let (r2_id, r2_node) = embedded_fact(&store, &topic_ent, topic_node, "second root", NEAR);
    let (e1_id, e1_node) = embedded_fact(&store, &topic_ent, topic_node, "first evidence", FAR);
    let (e2_id, e2_node) = embedded_fact(&store, &topic_ent, topic_node, "second evidence", FAR);
    support(&store, &e1_id, &r1_id);
    support(&store, &e2_id, &r2_id);

    let hits = store
        .vector_score_state_expanded(
            SearchKind::Fact,
            &query(NEAR),
            CandidateSet::CurrentSupportFacts,
            &[r1_node, r2_node],
            ExpandEdge::Supports,
            ExpandDirection::Incoming,
            SetOp::Intersection,
            10,
        )
        .expect("expanded score");
    let got = nodes_of(&hits);
    assert!(
        got.contains(&e1_node) && got.contains(&e2_node),
        "both roots' evidence surface from one expansion: {hits:?}",
    );

    // k caps the ranking: the two NEAR roots are the nearest of the four, so k=1 yields one
    // hit and it is a root, not a far evidence fact.
    let capped = store
        .vector_score_state_expanded(
            SearchKind::Fact,
            &query(NEAR),
            CandidateSet::CurrentSupportFacts,
            &[r1_node, r2_node],
            ExpandEdge::Supports,
            ExpandDirection::Incoming,
            SetOp::Intersection,
            1,
        )
        .expect("expanded score");
    assert_eq!(capped.len(), 1, "k bounds the expanded ranking");
    assert!(
        capped[0].node == r1_node || capped[0].node == r2_node,
        "the single capped hit is the nearest root, not a far evidence fact",
    );
}
