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

/// A `ReliabilityUpdate` audit event keyed by `marker`, subject = the agent whose score moved.
fn reliability_event(agent: Id, marker: &str) -> AuditEvent {
    AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(marker.as_bytes()),
            ingested_at: ts(NOW),
            namespace: Namespace::System,
            expired_at: None,
        },
        kind: AuditKind::ReliabilityUpdate,
        subject_id: agent,
        actor_id: Id::from_content_hash(b"substrate"),
        payload: serde_json::json!({
            "category": "reliability", "outcome": "failure", "weight": 1.0, "role": "producer"
        }),
        signature: String::new(),
        occurred_at: ts(NOW),
    }
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
