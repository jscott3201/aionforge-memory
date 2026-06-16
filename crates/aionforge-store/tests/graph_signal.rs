//! Personalized PageRank associative signal (M3.T01) — L0 acceptance.
//!
//! Builds a tiny associative graph — an `Entity` with two `Fact`s `ABOUT` it and one
//! `Episode` that `MENTIONS` it — and seeds PageRank on the entity. The schema points
//! records *at* entities (`Fact -ABOUT-> Entity`, `Episode -MENTIONS-> Entity`), so
//! under natural directed PageRank the seed entity is a sink and its mass never reaches
//! the records. These tests prove two things the signal depends on: the `undirected`
//! orientation spreads the seed's mass to the connected facts and episodes, and the
//! per-kind label filter returns only the requested kind.

mod common;

use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::value::ObjectValue;

use aionforge_store::{BoundQuery, NodeId, QueryResult, SearchHit, SearchKind, Store, Value};

use common::{
    entity, fact, identity, insert_scope, open_window, stats, store, support_edge, ts, zdt,
};

/// A minimal raw episode (no embedding — PageRank ignores vectors entirely).
fn episode(content: &str) -> Episode {
    Episode {
        identity: identity(Id::generate()),
        stats: stats(),
        content: content.to_string(),
        role: Role::User,
        captured_at: ts("2026-06-06T09:29:59-05:00[America/Chicago]"),
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    }
}

/// Wire an `Episode -MENTIONS-> Entity` edge by domain id (no typed writer exists yet;
/// this is a real commit through the parameter-bound write path, like the grounding
/// fixtures). Both `NOT NULL` timestamp props on the edge are bound.
fn mention(store: &Store, episode_id: &Id, entity_id: &Id) {
    let query = BoundQuery::new(
        "MATCH (e:Episode {id: $from}), (n:Entity {id: $to}) \
         INSERT (e)-[:MENTIONS {valid_from: $ts, ingested_at: $ts}]->(n)",
    )
    .bind_uuid("from", episode_id)
    .unwrap()
    .bind_uuid("to", entity_id)
    .unwrap()
    .bind("ts", zdt())
    .unwrap();
    store.execute(&query).expect("insert MENTIONS edge");
}

/// The matched nodes of a hit list, sorted, for set comparison.
fn nodes_of(hits: &[SearchHit]) -> Vec<NodeId> {
    let mut nodes: Vec<NodeId> = hits.iter().map(|hit| hit.node).collect();
    nodes.sort();
    nodes
}

/// Sorted node list, so expected and actual compare as sets regardless of score order.
fn sorted(mut nodes: Vec<NodeId>) -> Vec<NodeId> {
    nodes.sort();
    nodes
}

/// The engine node id of the `Scope` with domain id `id`. `Scope` sits outside the
/// associative projection (which spans only `Entity`/`Fact`/`Episode`), so its node id is
/// what the seed-not-in-projection error path needs.
fn scope_node(store: &Store, id: &Id) -> NodeId {
    let query = BoundQuery::new("MATCH (s:Scope {id: $id}) RETURN s AS node_id")
        .bind_uuid("id", id)
        .unwrap();
    let QueryResult::Rows(rows) = store.execute(&query).expect("match scope") else {
        panic!("a MATCH ... RETURN yields rows");
    };
    let idx = rows.column_index("node_id").expect("node_id column");
    match rows.value(0, idx).expect("one scope row") {
        Value::NodeRef(node) => *node,
        other => panic!("expected a node ref, got {other:?}"),
    }
}

#[test]
fn undirected_pagerank_spreads_entity_seed_and_filters_by_kind() {
    let store = store();

    // The entity the query names; keep its domain id for the MENTIONS edge below.
    let ent = entity("aionforge");
    let entity_id = ent.identity.id;
    let e_node = store.insert_entity(&ent).expect("insert entity");

    // Two facts ABOUT the entity (assert_fact writes the Fact and the ABOUT edge).
    let f1 = store
        .assert_fact(
            &fact(
                ent.identity.id,
                "is",
                ObjectValue::Text("a memory substrate".to_string()),
                "aionforge is a memory substrate",
            ),
            e_node,
            &open_window("2026-06-06T09:30:00-05:00[America/Chicago]"),
        )
        .expect("assert fact 1");
    let f2 = store
        .assert_fact(
            &fact(
                ent.identity.id,
                "uses",
                ObjectValue::Text("selene".to_string()),
                "aionforge uses selene",
            ),
            e_node,
            &open_window("2026-06-06T09:31:00-05:00[America/Chicago]"),
        )
        .expect("assert fact 2");

    // One episode that MENTIONS the entity.
    let ep = episode("we talked about aionforge today");
    let ep_id = ep.identity.id;
    let ep_node = store.insert_episode(&ep).expect("insert episode");
    mention(&store, &ep_id, &entity_id);

    // Facts: the seed's mass reaches both facts (positive — directed PR would strand
    // them at 0), and the kind filter returns exactly the two facts, no entity/episode.
    let fact_hits = store
        .personalized_pagerank(SearchKind::Fact, &[e_node], 10)
        .expect("pagerank facts");
    assert_eq!(
        nodes_of(&fact_hits),
        sorted(vec![f1, f2]),
        "the Fact filter returns exactly the two facts ABOUT the seed",
    );
    assert!(
        fact_hits.iter().all(|hit| hit.score > 0.0),
        "undirected spreading gives the facts positive mass: {fact_hits:?}",
    );

    // Episodes: only the mentioning episode, with positive mass.
    let episode_hits = store
        .personalized_pagerank(SearchKind::Episode, &[e_node], 10)
        .expect("pagerank episodes");
    assert_eq!(
        nodes_of(&episode_hits),
        vec![ep_node],
        "the Episode filter returns only the mentioning episode",
    );
    assert!(
        episode_hits[0].score > 0.0,
        "the episode gets positive mass"
    );

    // Entities: only the seed entity itself.
    let entity_hits = store
        .personalized_pagerank(SearchKind::Entity, &[e_node], 10)
        .expect("pagerank entities");
    assert_eq!(
        nodes_of(&entity_hits),
        vec![e_node],
        "the Entity filter returns only the seed entity",
    );
}

#[test]
fn result_nodes_scopes_the_ranking_to_the_visible_set() {
    let store = store();

    // One entity with two facts ABOUT it; the seed reaches both (undirected spreading).
    let ent = entity("aionforge");
    let e_node = store.insert_entity(&ent).expect("insert entity");
    let f1 = store
        .assert_fact(
            &fact(
                ent.identity.id,
                "is",
                ObjectValue::Text("a memory substrate".to_string()),
                "aionforge is a memory substrate",
            ),
            e_node,
            &open_window("2026-06-06T09:30:00-05:00[America/Chicago]"),
        )
        .expect("assert fact 1");
    let f2 = store
        .assert_fact(
            &fact(
                ent.identity.id,
                "uses",
                ObjectValue::Text("selene".to_string()),
                "aionforge uses selene",
            ),
            e_node,
            &open_window("2026-06-06T09:31:00-05:00[America/Chicago]"),
        )
        .expect("assert fact 2");

    // `None`: unscoped — both facts rank (the baseline this scopes down from).
    let unscoped = store
        .personalized_pagerank_within(SearchKind::Fact, &[e_node], 10, None, None)
        .expect("unscoped");
    assert_eq!(
        nodes_of(&unscoped),
        sorted(vec![f1, f2]),
        "None ranks every in-projection fact the seed reaches",
    );

    // `Some(scope)`: the ranking is restricted to the scope, even with `k` large enough for
    // both facts — so f2 is dropped purely because it is out of scope, not truncated away.
    // (The selene layer proves the intersection runs *before* the top-k truncation:
    // `pagerank_result_nodes_intersects_before_limit`.)
    let scoped = store
        .personalized_pagerank_within(SearchKind::Fact, &[e_node], 10, Some(&[f1]), None)
        .expect("scoped to f1");
    assert_eq!(
        nodes_of(&scoped),
        vec![f1],
        "result_nodes restricts the ranking to the in-scope fact",
    );
    assert!(
        scoped.iter().all(|hit| hit.score > 0.0),
        "the in-scope fact keeps its positive personalized mass: {scoped:?}",
    );

    // `Some(empty)`: nothing is in scope (the reader has no visible record) — an empty
    // ranking, distinct from `None`'s unscoped ranking.
    let empty_scope = store
        .personalized_pagerank_within(SearchKind::Fact, &[e_node], 10, Some(&[]), None)
        .expect("empty scope");
    assert!(
        empty_scope.is_empty(),
        "Some(empty) yields an empty ranking, not the unscoped one: {empty_scope:?}",
    );
}

#[test]
fn mass_spreads_across_support_chains_and_skips_disconnected_facts() {
    let store = store();

    // Seed entity E1 with a direct fact F1; F1 SUPPORTS F2, and F2 is ABOUT a second
    // entity E2. A disconnected fact F3 (about an un-seeded E3, no support link) shares
    // nothing with the seed's component.
    let e1 = entity("aionforge");
    let e1_node = store.insert_entity(&e1).expect("insert e1");
    let e2 = entity("selene");
    let e2_node = store.insert_entity(&e2).expect("insert e2");
    let e3 = entity("unrelated");
    let e3_node = store.insert_entity(&e3).expect("insert e3");

    let f1 = fact(
        e1.identity.id,
        "uses",
        ObjectValue::Text("selene".to_string()),
        "aionforge uses selene",
    );
    let f1_id = f1.identity.id;
    let f1_node = store
        .assert_fact(
            &f1,
            e1_node,
            &open_window("2026-06-06T09:30:00-05:00[America/Chicago]"),
        )
        .expect("assert f1");

    let f2 = fact(
        e2.identity.id,
        "is",
        ObjectValue::Text("a graph engine".to_string()),
        "selene is a graph engine",
    );
    let f2_id = f2.identity.id;
    let f2_node = store
        .assert_fact(
            &f2,
            e2_node,
            &open_window("2026-06-06T09:31:00-05:00[America/Chicago]"),
        )
        .expect("assert f2");

    let f3 = fact(
        e3.identity.id,
        "is",
        ObjectValue::Text("off topic".to_string()),
        "unrelated is off topic",
    );
    let f3_node = store
        .assert_fact(
            &f3,
            e3_node,
            &open_window("2026-06-06T09:32:00-05:00[America/Chicago]"),
        )
        .expect("assert f3");

    support_edge(&store, &f1_id, &f2_id);

    // Seeding on E1 alone reaches F2 two hops away (E1 -ABOUT- F1 -SUPPORTS-> F2): the
    // associative, multi-hop behavior the signal exists for. F3, in its own component,
    // never appears.
    let fact_hits = store
        .personalized_pagerank(SearchKind::Fact, &[e1_node], 10)
        .expect("pagerank facts");
    assert_eq!(
        nodes_of(&fact_hits),
        sorted(vec![f1_node, f2_node]),
        "the seed reaches F1 directly and F2 across the support chain, but not F3",
    );
    assert!(
        !nodes_of(&fact_hits).contains(&f3_node),
        "a fact in a disconnected component is excluded",
    );

    // The direct fact outranks the one reached only through the support hop.
    let rank_of = |node: NodeId| fact_hits.iter().position(|hit| hit.node == node);
    assert!(
        rank_of(f1_node) < rank_of(f2_node),
        "the directly-attached fact outranks the support-chained one: {fact_hits:?}",
    );

    // Truncation respects that ranking: k=1 keeps the top-ranked F1, not the lower
    // support-chained F2 — the cap drops the tail of the order, not an arbitrary hit.
    let top_fact = store
        .personalized_pagerank(SearchKind::Fact, &[e1_node], 1)
        .expect("pagerank top fact");
    assert_eq!(
        nodes_of(&top_fact),
        vec![f1_node],
        "k=1 keeps the highest-ranked fact, the directly-attached F1",
    );

    // The chain also carries mass to E2; E3 stays out.
    let entity_hits = store
        .personalized_pagerank(SearchKind::Entity, &[e1_node], 10)
        .expect("pagerank entities");
    assert_eq!(
        nodes_of(&entity_hits),
        sorted(vec![e1_node, e2_node]),
        "mass reaches the chained entity E2 but never the disconnected E3",
    );
}

#[test]
fn empty_seeds_yield_an_empty_ranking() {
    let store = store();
    let ent = entity("aionforge");
    let e_node = store.insert_entity(&ent).expect("insert entity");
    store
        .assert_fact(
            &fact(
                ent.identity.id,
                "is",
                ObjectValue::Text("a memory substrate".to_string()),
                "aionforge is a memory substrate",
            ),
            e_node,
            &open_window("2026-06-06T09:30:00-05:00[America/Chicago]"),
        )
        .expect("assert fact");

    // A graph signal needs a personalization root; no seeds means no associative prior,
    // not a uniform PageRank fallback.
    assert!(
        store
            .personalized_pagerank(SearchKind::Fact, &[], 10)
            .expect("empty seeds are not an error")
            .is_empty(),
        "empty seeds yield an empty ranking",
    );
}

#[test]
fn k_bounds_the_ranking() {
    let store = store();
    let ent = entity("aionforge");
    let e_node = store.insert_entity(&ent).expect("insert entity");
    let mut fact_nodes = Vec::new();
    for n in 0..4 {
        fact_nodes.push(
            store
                .assert_fact(
                    &fact(
                        ent.identity.id,
                        "rel",
                        ObjectValue::Text(format!("object {n}")),
                        &format!("statement {n}"),
                    ),
                    e_node,
                    &open_window("2026-06-06T09:30:00-05:00[America/Chicago]"),
                )
                .expect("assert fact"),
        );
    }

    let hits = store
        .personalized_pagerank(SearchKind::Fact, &[e_node], 2)
        .expect("pagerank facts");
    assert_eq!(
        hits.len(),
        2,
        "k caps the returned hits at 2 of the 4 facts"
    );
    assert!(
        hits.iter().all(|hit| fact_nodes.contains(&hit.node)),
        "the capped hits are drawn from the four facts: {hits:?}",
    );

    // The four facts are structurally symmetric, so PageRank scores them alike and the
    // node-id tiebreak decides the order. That order is deterministic: a repeated call
    // returns the same hits in the same sequence, so the cap is stable, not arbitrary.
    let order = |hits: &[SearchHit]| hits.iter().map(|hit| hit.node).collect::<Vec<_>>();
    let again = store
        .personalized_pagerank(SearchKind::Fact, &[e_node], 2)
        .expect("pagerank facts again");
    assert_eq!(
        order(&hits),
        order(&again),
        "repeated calls return the same hits in the same order",
    );
}

#[test]
fn zero_k_yields_an_empty_ranking() {
    let store = store();
    let ent = entity("aionforge");
    let e_node = store.insert_entity(&ent).expect("insert entity");
    store
        .assert_fact(
            &fact(
                ent.identity.id,
                "is",
                ObjectValue::Text("a memory substrate".to_string()),
                "aionforge is a memory substrate",
            ),
            e_node,
            &open_window("2026-06-06T09:30:00-05:00[America/Chicago]"),
        )
        .expect("assert fact");

    // k = 0 asks for no hits; the guard short-circuits to empty without touching the
    // graph (the twin of the empty-seeds guard), so it is never a degenerate query.
    assert!(
        store
            .personalized_pagerank(SearchKind::Fact, &[e_node], 0)
            .expect("k=0 is not an error")
            .is_empty(),
        "k=0 yields an empty ranking",
    );
}

#[test]
fn a_kind_with_no_nodes_yields_an_empty_ranking() {
    let store = store();
    let ent = entity("aionforge");
    let e_node = store.insert_entity(&ent).expect("insert entity");
    store
        .assert_fact(
            &fact(
                ent.identity.id,
                "is",
                ObjectValue::Text("a memory substrate".to_string()),
                "aionforge is a memory substrate",
            ),
            e_node,
            &open_window("2026-06-06T09:30:00-05:00[America/Chicago]"),
        )
        .expect("assert fact");

    // The graph holds an entity and a fact but no notes. Asking for the Note kind
    // intersects the PageRank scores with an empty node set, so the ranking is empty —
    // a kind the graph has no instances of is not an error, just no hits.
    assert!(
        store
            .personalized_pagerank(SearchKind::Note, &[e_node], 10)
            .expect("an unrepresented kind is not an error")
            .is_empty(),
        "a kind with no nodes in the graph yields an empty ranking",
    );
}

#[test]
fn multiple_seeds_reach_each_seed_component() {
    let store = store();

    // Two unconnected entities, each with one fact ABOUT it — no edge bridges them.
    let e1 = entity("aionforge");
    let e1_node = store.insert_entity(&e1).expect("insert e1");
    let e2 = entity("selene");
    let e2_node = store.insert_entity(&e2).expect("insert e2");

    let f1_node = store
        .assert_fact(
            &fact(
                e1.identity.id,
                "is",
                ObjectValue::Text("a memory substrate".to_string()),
                "aionforge is a memory substrate",
            ),
            e1_node,
            &open_window("2026-06-06T09:30:00-05:00[America/Chicago]"),
        )
        .expect("assert f1");
    let f2_node = store
        .assert_fact(
            &fact(
                e2.identity.id,
                "is",
                ObjectValue::Text("a graph engine".to_string()),
                "selene is a graph engine",
            ),
            e2_node,
            &open_window("2026-06-06T09:31:00-05:00[America/Chicago]"),
        )
        .expect("assert f2");

    // Seeding both entities spreads restart mass into both otherwise-disconnected
    // components, so each seed's fact comes back — a single seed would reach only its
    // own. The personalization list carries both roots at equal weight.
    let fact_hits = store
        .personalized_pagerank(SearchKind::Fact, &[e1_node, e2_node], 10)
        .expect("pagerank facts");
    assert_eq!(
        nodes_of(&fact_hits),
        sorted(vec![f1_node, f2_node]),
        "both seeds' facts are reached when both entities seed the walk",
    );
    assert!(
        fact_hits.iter().all(|hit| hit.score > 0.0),
        "each seeded component's fact carries positive mass: {fact_hits:?}",
    );
}

#[test]
fn a_seed_outside_the_projection_is_an_error() {
    let store = store();

    // An entity that is in the projection, plus a Scope node that is not (the projection
    // spans only Entity/Fact/Episode).
    let ent = entity("aionforge");
    store.insert_entity(&ent).expect("insert entity");
    let scope_id = Id::generate();
    insert_scope(&store, &scope_id);
    let scope = scope_node(&store, &scope_id);

    // selene validates that every personalization seed is a projected node and rejects
    // one that is not; the store surfaces that as an error rather than a silent empty —
    // a seed the projection can't place is a caller bug, not a no-op.
    assert!(
        store
            .personalized_pagerank(SearchKind::Fact, &[scope], 10)
            .is_err(),
        "seeding on a node outside the associative projection is an error",
    );
}
