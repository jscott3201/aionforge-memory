//! Node-level acceptance for the candidate-state sets whose membership depends on edges
//! the typed M2.T01 ops do not write (data-model §9, M2.T02): `provenance_current_support_facts`
//! (incoming SUPPORTS + outgoing HAS_PROVENANCE), `scope_membership` (live IN_SCOPE), and
//! `recency_active` (live RECENT_IN). The edges and their endpoint nodes are wired through
//! the engine's parameter-bound write path until their own typed writers land
//! (M2.T04/T05/T08). Shared fixtures live in `common/mod.rs`; the supersession/contradiction
//! membership tests are in `providers.rs`.

mod common;
use common::*;

use aionforge_domain::ids::Id;
use aionforge_domain::value::ObjectValue;
use aionforge_store::CandidateSet;

#[test]
fn provenance_current_support_facts_includes_grounded_facts() {
    // The inclusion direction of provenance_current_support_facts: a current fact joins
    // the grounded set exactly when it gains an incoming SUPPORTS and an outgoing
    // HAS_PROVENANCE. An otherwise-current but ungrounded fact stays out.
    let store = store();
    let subject = entity("release");
    let subject_node = store.insert_entity(&subject).expect("insert entity");
    let grounded = fact(
        subject.identity.id,
        "shipped_on",
        ObjectValue::Text("friday".to_string()),
        "the release shipped on friday",
    );
    let supporter = fact(
        subject.identity.id,
        "noted_by",
        ObjectValue::Text("changelog".to_string()),
        "the changelog records the ship date",
    );
    let grounded_node = store
        .assert_fact(
            &grounded,
            subject_node,
            &open_window("2026-06-06T00:00:00Z[UTC]"),
        )
        .expect("assert grounded");
    let supporter_node = store
        .assert_fact(
            &supporter,
            subject_node,
            &open_window("2026-06-06T00:00:00Z[UTC]"),
        )
        .expect("assert supporter");

    // Before grounding, neither fact is in the provenance set (no SUPPORTS, no provenance).
    let before = members(&store, CandidateSet::ProvenanceCurrentSupportFacts);
    assert!(
        !before.contains(&grounded_node) && !before.contains(&supporter_node),
        "ungrounded facts are excluded from provenance_current_support_facts"
    );

    // Ground it: supporter -SUPPORTS-> grounded, and grounded -HAS_PROVENANCE-> record.
    let prov_id = Id::generate();
    insert_provenance(&store, &prov_id, &grounded.identity.id);
    insert_edge(
        &store,
        "MATCH (a:Fact {id: $from}), (b:Fact {id: $to}) \
         INSERT (a)-[:SUPPORTS {weight: 1.0}]->(b)",
        &supporter.identity.id,
        &grounded.identity.id,
    );
    insert_edge(
        &store,
        "MATCH (a:Fact {id: $from}), (b:ProvenanceRecord {id: $to}) \
         INSERT (a)-[:HAS_PROVENANCE]->(b)",
        &grounded.identity.id,
        &prov_id,
    );

    let after = members(&store, CandidateSet::ProvenanceCurrentSupportFacts);
    assert!(
        after.contains(&grounded_node),
        "a fact with incoming SUPPORTS and outgoing HAS_PROVENANCE joins the grounded set"
    );
    assert!(
        !after.contains(&supporter_node),
        "the supporter has an outgoing (not incoming) SUPPORTS and no provenance, so it stays out"
    );
    // It remains in the plain current set too — grounding is an addition, not a move.
    assert!(
        members(&store, CandidateSet::CurrentSupportFacts).contains(&grounded_node),
        "grounding does not remove a fact from current_support_facts"
    );
}

#[test]
fn scope_membership_contains_only_the_scoped_node() {
    // scope_membership is the coarse "in some scope" set: any node with a live IN_SCOPE
    // edge. Proven at node level — the scoped fact is a member, an unscoped one is not.
    let store = store();
    let subject = entity("topic");
    let subject_node = store.insert_entity(&subject).expect("insert entity");
    let scoped = fact(
        subject.identity.id,
        "is",
        ObjectValue::Text("in-scope".to_string()),
        "this fact is in scope",
    );
    let unscoped = fact(
        subject.identity.id,
        "is",
        ObjectValue::Text("out-of-scope".to_string()),
        "this fact is not in scope",
    );
    let scoped_node = store
        .assert_fact(
            &scoped,
            subject_node,
            &open_window("2026-06-06T00:00:00Z[UTC]"),
        )
        .expect("assert scoped");
    let unscoped_node = store
        .assert_fact(
            &unscoped,
            subject_node,
            &open_window("2026-06-06T00:00:00Z[UTC]"),
        )
        .expect("assert unscoped");

    assert!(
        members(&store, CandidateSet::ScopeMembership).is_empty(),
        "no IN_SCOPE edges yet, so scope_membership is empty"
    );
    let scope_id = Id::generate();
    insert_scope(&store, &scope_id);
    insert_edge(
        &store,
        "MATCH (a:Fact {id: $from}), (b:Scope {id: $to}) INSERT (a)-[:IN_SCOPE]->(b)",
        &scoped.identity.id,
        &scope_id,
    );

    let scope_members = members(&store, CandidateSet::ScopeMembership);
    assert!(
        scope_members.contains(&scoped_node),
        "the scoped fact is a scope member"
    );
    assert!(
        !scope_members.contains(&unscoped_node),
        "a fact with no IN_SCOPE edge is not a scope member"
    );
}

#[test]
fn recency_active_contains_only_the_recent_node() {
    // recency_active is the coarse freshness set: any node with a live RECENT_IN edge.
    let store = store();
    let subject = entity("event");
    let subject_node = store.insert_entity(&subject).expect("insert entity");
    let recent = fact(
        subject.identity.id,
        "happened",
        ObjectValue::Text("now".to_string()),
        "this happened recently",
    );
    let stale = fact(
        subject.identity.id,
        "happened",
        ObjectValue::Text("long ago".to_string()),
        "this happened a while back",
    );
    let recent_node = store
        .assert_fact(
            &recent,
            subject_node,
            &open_window("2026-06-06T00:00:00Z[UTC]"),
        )
        .expect("assert recent");
    let stale_node = store
        .assert_fact(
            &stale,
            subject_node,
            &open_window("2026-06-06T00:00:00Z[UTC]"),
        )
        .expect("assert stale");

    assert!(
        members(&store, CandidateSet::RecencyActive).is_empty(),
        "no RECENT_IN edges yet, so recency_active is empty"
    );
    let window_id = Id::generate();
    insert_recency_window(&store, &window_id);
    insert_edge(
        &store,
        "MATCH (a:Fact {id: $from}), (b:RecencyWindow {id: $to}) INSERT (a)-[:RECENT_IN]->(b)",
        &recent.identity.id,
        &window_id,
    );

    let recency_members = members(&store, CandidateSet::RecencyActive);
    assert!(
        recency_members.contains(&recent_node),
        "the fact in the recency window is recency-active"
    );
    assert!(
        !recency_members.contains(&stale_node),
        "a fact with no RECENT_IN edge is not recency-active"
    );
}
