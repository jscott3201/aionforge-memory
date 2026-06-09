//! L0 acceptance for the trust-scoring fold surface (06 §5, M4.T05).
//!
//! These are the store mechanics the L2 fold drives: attributing an invalidated fact to its
//! distinct producing agents, recording an idempotent `ReliabilityUpdate` event, and refreshing
//! the recomputable caches (`Agent.trust_scores`, `Fact.stats.trust`) write-when-changed. The fold
//! math and the trigger policy live in L2 and are tested there.

mod common;

use std::collections::BTreeMap;

use common::{assert_about, entity, fact, identity, open_window, store, ts, zdt};

use aionforge_domain::blocks::Identity;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::agent::{Agent, AgentStatus, TrustCategory, TrustScores};
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::semantic::FactStatus;
use aionforge_domain::value::ObjectValue;
use aionforge_store::{BoundQuery, NodeId, Store};

const NOW: &str = "2026-06-08T09:00:00-05:00[America/Chicago]";
const FROM: &str = "2026-06-08T08:00:00-05:00[America/Chicago]";

/// Insert a raw `Episode` captured by `agent`, returning its domain id.
fn insert_episode(store: &Store, agent: Id, seed: &[u8]) -> Id {
    let id = Id::generate();
    let episode = Episode {
        identity: identity(id),
        stats: common::stats(),
        content: "source".to_string(),
        role: Role::User,
        captured_at: ts(NOW),
        agent_id: agent,
        session_id: None,
        content_hash: ContentHash::of(seed),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("insert episode");
    id
}

/// Insert a raw `Episode` captured by `agent` with a custom write-time `trust` (the baseline the
/// reliability recompute anchors to).
fn insert_episode_with_trust(store: &Store, agent: Id, seed: &[u8], trust: f64) -> Id {
    let id = Id::generate();
    let mut stats = common::stats();
    stats.trust = trust;
    let episode = Episode {
        identity: identity(id),
        stats,
        content: "source".to_string(),
        role: Role::User,
        captured_at: ts(NOW),
        agent_id: agent,
        session_id: None,
        content_hash: ContentHash::of(seed),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("insert episode");
    id
}

/// Wire a `DERIVED_FROM` edge fact → episode, the production attribution shape (the edge carries
/// a required `derived_at`).
fn derive(store: &Store, fact_id: &Id, episode_id: &Id) {
    let query = BoundQuery::new(
        "MATCH (a:Fact {id: $from}), (b:Episode {id: $to}) \
         INSERT (a)-[:DERIVED_FROM {derived_at: $ts}]->(b)",
    )
    .bind_uuid("from", fact_id)
    .expect("bind from")
    .bind_uuid("to", episode_id)
    .expect("bind to")
    .bind("ts", zdt())
    .expect("bind ts");
    store.execute(&query).expect("derive edge");
}

/// Enroll an agent with one category scored `score`, returning its domain id.
fn enroll(store: &Store, category: &str, score: f64) -> Id {
    let id = Id::generate();
    let mut scores = BTreeMap::new();
    scores.insert(
        category.to_string(),
        TrustCategory {
            alpha: 1.0,
            beta: 1.0,
            score,
        },
    );
    let agent = Agent {
        identity: identity(id),
        public_key: "cHVibGljLWtleQ==".to_string(),
        model_family: "test".to_string(),
        model_version: None,
        trust_scores: TrustScores(scores),
        status: AgentStatus::Active,
    };
    store.create_agent(&agent).expect("enroll agent");
    id
}

/// An `AuditEvent` of a given `kind` keyed by `marker`, subject = the agent it concerns.
fn audit_event(agent: Id, marker: &str, kind: AuditKind) -> AuditEvent {
    AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(marker.as_bytes()),
            ingested_at: ts(NOW),
            namespace: Namespace::System,
            expired_at: None,
        },
        kind,
        subject_id: agent,
        actor_id: Id::from_content_hash(b"substrate"),
        payload: serde_json::json!({
            "category": "reliability", "outcome": "failure", "weight": 1.0, "role": "producer"
        }),
        signature: String::new(),
        occurred_at: ts(NOW),
    }
}

/// A `ReliabilityUpdate` audit event keyed by `marker`, subject = the agent whose score moved.
fn reliability_event(agent: Id, marker: &str) -> AuditEvent {
    audit_event(agent, marker, AuditKind::ReliabilityUpdate)
}

/// Wire a `DERIVED_FROM` edge fact → a non-`Episode` `Fact` target. `DERIVED_FROM` is polymorphic
/// (catalog.rs has no `FROM`/`TO` clause), so a `Fact` is a valid endpoint that carries no
/// `agent_id` — exactly the producing-agent skip case.
fn derive_to_fact(store: &Store, fact_id: &Id, target_fact_id: &Id) {
    let query = BoundQuery::new(
        "MATCH (a:Fact {id: $from}), (b:Fact {id: $to}) \
         INSERT (a)-[:DERIVED_FROM {derived_at: $ts}]->(b)",
    )
    .bind_uuid("from", fact_id)
    .expect("bind from")
    .bind_uuid("to", target_fact_id)
    .expect("bind to")
    .bind("ts", zdt())
    .expect("bind ts");
    store.execute(&query).expect("derive edge to fact");
}

#[test]
fn producing_agents_are_distinct_and_id_sorted() {
    let store = store();
    let subject = entity("graph databases");
    let f = fact(
        subject.identity.id,
        "preferred_by",
        ObjectValue::Text("the team".to_string()),
        "the team prefers graph databases",
    );
    let fact_node: NodeId = assert_about(&store, &subject, &f, &open_window(FROM));

    // One agent supported the fact from two distinct episodes; a second agent from a third.
    let ada = Id::generate();
    let bo = Id::generate();
    let ep1 = insert_episode(&store, ada, b"ep1");
    let ep2 = insert_episode(&store, ada, b"ep2");
    let ep3 = insert_episode(&store, bo, b"ep3");
    derive(&store, &f.identity.id, &ep1);
    derive(&store, &f.identity.id, &ep2);
    derive(&store, &f.identity.id, &ep3);

    let mut expected = vec![ada, bo];
    expected.sort_unstable();
    assert_eq!(
        store.producing_agents(fact_node).expect("producing agents"),
        expected,
        "one agent over two source episodes counts once; the set is distinct and id-sorted"
    );
}

#[test]
fn a_fact_with_no_derivation_has_no_producers() {
    let store = store();
    let subject = entity("orphan");
    let f = fact(
        subject.identity.id,
        "is",
        ObjectValue::Text("unsourced".to_string()),
        "an unsourced claim",
    );
    let fact_node = assert_about(&store, &subject, &f, &open_window(FROM));
    assert!(
        store
            .producing_agents(fact_node)
            .expect("producing agents")
            .is_empty(),
        "no DERIVED_FROM edges means no attributable producers"
    );
}

#[test]
fn facts_produced_by_lists_an_agents_distinct_facts() {
    let store = store();
    // Fact f is produced by ada (one episode) and bo (another).
    let subj_f = entity("graph databases");
    let f = fact(
        subj_f.identity.id,
        "preferred_by",
        ObjectValue::Text("the team".to_string()),
        "the team prefers graph databases",
    );
    let f_node: NodeId = assert_about(&store, &subj_f, &f, &open_window(FROM));
    // Fact g is produced by ada alone, across two episodes (the dedup case).
    let subj_g = entity("rust");
    let g = fact(
        subj_g.identity.id,
        "is",
        ObjectValue::Text("fast".to_string()),
        "rust is fast",
    );
    let g_node: NodeId = assert_about(&store, &subj_g, &g, &open_window(FROM));

    let ada = Id::generate();
    let bo = Id::generate();
    let f_ep_ada = insert_episode(&store, ada, b"f-ada");
    let f_ep_bo = insert_episode(&store, bo, b"f-bo");
    let g_ep1 = insert_episode(&store, ada, b"g-ada-1");
    let g_ep2 = insert_episode(&store, ada, b"g-ada-2");
    derive(&store, &f.identity.id, &f_ep_ada);
    derive(&store, &f.identity.id, &f_ep_bo);
    derive(&store, &g.identity.id, &g_ep1);
    derive(&store, &g.identity.id, &g_ep2);

    let ada_facts = store.facts_produced_by(&ada).expect("ada facts");
    assert_eq!(
        ada_facts.len(),
        2,
        "ada produced f and g; g over two episodes counts once"
    );
    assert!(ada_facts.contains(&f_node) && ada_facts.contains(&g_node));

    assert_eq!(
        store.facts_produced_by(&bo).expect("bo facts"),
        vec![f_node],
        "bo produced only f"
    );

    assert!(
        store
            .facts_produced_by(&Id::generate())
            .expect("unknown agent")
            .is_empty(),
        "an agent with no episodes produced no facts"
    );
}

#[test]
fn fact_source_trust_mean_is_the_id_sorted_mean_of_source_episode_trust() {
    let store = store();
    let subject = entity("graph databases");
    let f = fact(
        subject.identity.id,
        "preferred_by",
        ObjectValue::Text("the team".to_string()),
        "the team prefers graph databases",
    );
    let fact_node: NodeId = assert_about(&store, &subject, &f, &open_window(FROM));

    // No source episode yet ⇒ no baseline.
    assert!(
        store
            .fact_source_trust_mean(fact_node)
            .expect("baseline")
            .is_none(),
        "a fact with no episode source has no baseline"
    );

    // Two source episodes with trust 0.6 and 1.0 ⇒ mean 0.8, independent of edge order.
    let ada = Id::generate();
    let ep_high = insert_episode_with_trust(&store, ada, b"hi", 1.0);
    let ep_low = insert_episode_with_trust(&store, ada, b"lo", 0.6);
    derive(&store, &f.identity.id, &ep_high);
    derive(&store, &f.identity.id, &ep_low);

    let baseline = store
        .fact_source_trust_mean(fact_node)
        .expect("baseline")
        .expect("two sources");
    assert!((baseline - 0.8).abs() < 1e-9, "mean of 0.6 and 1.0 is 0.8");
}

#[test]
fn producing_agents_skips_a_derivation_target_without_an_agent_id() {
    let store = store();
    let subject = entity("graph databases");
    let f = fact(
        subject.identity.id,
        "preferred_by",
        ObjectValue::Text("the team".to_string()),
        "the team prefers graph databases",
    );
    let fact_node: NodeId = assert_about(&store, &subject, &f, &open_window(FROM));

    // Two real Episode producers carry an agent_id...
    let ada = Id::generate();
    let bo = Id::generate();
    let ep1 = insert_episode(&store, ada, b"skip-ep1");
    let ep2 = insert_episode(&store, bo, b"skip-ep2");
    derive(&store, &f.identity.id, &ep1);
    derive(&store, &f.identity.id, &ep2);

    // ...alongside an outgoing DERIVED_FROM to another Fact, a polymorphic endpoint that has no
    // agent_id. The skip branch (trust_fold.rs `else { continue }`) is dead in the graph today
    // (only fact->Episode is Fact-outgoing), so this pins it before a future Fact->X edge.
    let other = entity("a non-episode source");
    let g = fact(
        other.identity.id,
        "is",
        ObjectValue::Text("not an episode".to_string()),
        "a fact used as a non-episode derivation target",
    );
    assert_about(&store, &other, &g, &open_window(FROM));
    derive_to_fact(&store, &f.identity.id, &g.identity.id);

    let mut expected = vec![ada, bo];
    expected.sort_unstable();
    assert_eq!(
        store.producing_agents(fact_node).expect("producing agents"),
        expected,
        "the no-agent_id Fact target is skipped, not attributed and not an error"
    );
}

#[test]
fn recording_a_reliability_update_is_idempotent_by_event_id() {
    let store = store();
    let ada = enroll(&store, "reliability", 0.5);
    let event = reliability_event(ada, "reliability|fact-1|ada|reliability|producer");

    let first = store
        .record_reliability_update(&event)
        .expect("record once");
    let again = store
        .record_reliability_update(&event)
        .expect("record again");
    assert_eq!(
        first, again,
        "the replayed event reuses the same audit node"
    );

    let events = store.reliability_events(&ada).expect("read events");
    assert_eq!(events.len(), 1, "one agent's one event is recorded once");
    assert_eq!(events[0].kind, AuditKind::ReliabilityUpdate);
    assert_eq!(events[0].subject_id, ada);
}

#[test]
fn reliability_events_filters_to_the_agent_and_the_kind() {
    let store = store();
    let ada = enroll(&store, "reliability", 0.5);
    let bo = enroll(&store, "reliability", 0.5);
    store
        .record_reliability_update(&reliability_event(ada, "marker|ada|1"))
        .expect("ada event");
    store
        .record_reliability_update(&reliability_event(bo, "marker|bo|1"))
        .expect("bo event");

    assert_eq!(
        store.reliability_events(&ada).expect("ada").len(),
        1,
        "the by-subject read returns only this agent's events"
    );
    assert!(
        store
            .reliability_events(&Id::generate())
            .expect("unknown")
            .is_empty(),
        "an agent with no reliability events reads empty"
    );
}

#[test]
fn reliability_events_excludes_other_audit_kinds_for_the_same_subject() {
    let store = store();
    let ada = enroll(&store, "reliability", 0.5);

    // Two audits against the SAME subject through the same write path, differing only in kind.
    // subject_id is shared by every AuditEvent kind and the by-subject index is kind-agnostic, so
    // only the `kind == ReliabilityUpdate` filter (the M4.T06 by-subject replay seam) keeps the
    // Summarize out. `record_reliability_update` is mechanically kind-agnostic, so it seeds the
    // non-reliability event without a separate raw-audit write path.
    store
        .record_reliability_update(&reliability_event(ada, "ada|reliability"))
        .expect("reliability event");
    store
        .record_reliability_update(&audit_event(ada, "ada|summarize", AuditKind::Summarize))
        .expect("summarize event");

    let events = store.reliability_events(&ada).expect("read events");
    assert_eq!(
        events.len(),
        1,
        "the by-subject read keeps only ReliabilityUpdate kinds, not every audit for the subject"
    );
    assert_eq!(events[0].kind, AuditKind::ReliabilityUpdate);
}

#[test]
fn refresh_agent_trust_is_write_when_changed() {
    let store = store();
    let ada = enroll(&store, "reliability", 0.5);

    // Bump the cached category score and refresh.
    let mut agent = store.agent_by_id(&ada).expect("read").expect("agent");
    agent
        .trust_scores
        .0
        .get_mut("reliability")
        .expect("category")
        .score = 0.9;
    let node = store.refresh_agent_trust(&agent).expect("refresh");
    assert_eq!(
        store
            .agent_by_id(&ada)
            .expect("read")
            .expect("agent")
            .trust_scores
            .0
            .get("reliability")
            .expect("category")
            .score,
        0.9,
        "the cached score is updated"
    );

    // A second identical refresh is a no-op on the same node.
    let node_again = store.refresh_agent_trust(&agent).expect("refresh again");
    assert_eq!(
        node, node_again,
        "an unchanged refresh reuses the same node"
    );
}

#[test]
fn refresh_fact_trust_updates_the_node_summary() {
    let store = store();
    let subject = entity("topic");
    let f = fact(
        subject.identity.id,
        "rated",
        ObjectValue::Text("low".to_string()),
        "a fact whose producer decayed",
    );
    let fact_node = assert_about(&store, &subject, &f, &open_window(FROM));

    store
        .refresh_fact_trust(fact_node, 0.25)
        .expect("refresh trust");
    let stored = store
        .fact_by_node_id(fact_node)
        .expect("read")
        .expect("fact");
    assert_eq!(
        stored.stats.trust, 0.25,
        "the node-summary trust is updated"
    );
    assert_eq!(
        stored.status,
        FactStatus::Active,
        "only the trust summary changed; the fact is otherwise untouched"
    );

    // Idempotent: refreshing to the same value is a no-op.
    store
        .refresh_fact_trust(fact_node, 0.25)
        .expect("refresh same");
    assert_eq!(
        store
            .fact_by_node_id(fact_node)
            .expect("read")
            .expect("fact")
            .stats
            .trust,
        0.25
    );
}
