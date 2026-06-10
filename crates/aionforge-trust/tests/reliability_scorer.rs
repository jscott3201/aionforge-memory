//! Integration tests for the reliability scorer over a real store (06 §5, M4.T05).
//!
//! The pure recompute model and the event decode are unit-tested in the module; here we exercise
//! the wiring against a committed graph: the event builders read the right producers/attesters, the
//! distinct-author guard holds, and `apply` records events and refolds the agent and fact caches.

use std::sync::Arc;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::edges::About;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::agent::{Agent, AgentStatus, TrustScores};
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::value::ObjectValue;
use aionforge_store::{BoundQuery, NodeId, Store, StoreConfig};
use aionforge_trust::{ReliabilityPolicy, ReliabilityScorer};

const EPS: f64 = 1e-9;
const PREDICATE: &str = "preferred_by";

fn ts() -> Timestamp {
    "2026-06-08T09:30:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store.migrate(&ts()).expect("migrate");
    Arc::new(store)
}

fn scorer(store: Arc<Store>) -> ReliabilityScorer {
    ReliabilityScorer::new(
        store,
        ReliabilityPolicy {
            enabled: true,
            ..ReliabilityPolicy::default()
        },
    )
}

fn stats(trust: f64) -> Stats {
    Stats {
        importance: 0.5,
        trust,
        last_access: ts(),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

/// Enroll a fresh agent with empty trust scores; returns its id.
fn enroll(store: &Store) -> Id {
    let id = Id::generate();
    let agent = Agent {
        identity: Identity {
            id,
            ingested_at: ts(),
            namespace: Namespace::Agent("ops".to_string()),
            expired_at: None,
        },
        public_key: "dGVzdC1rZXk=".to_string(),
        model_family: "test".to_string(),
        model_version: None,
        trust_scores: TrustScores::default(),
        status: AgentStatus::Active,
    };
    store.create_agent(&agent).expect("enroll");
    id
}

/// Insert a raw episode captured by `agent` with write-time `trust`; returns its id.
fn episode(store: &Store, agent: Id, trust: f64, seed: u128) -> Id {
    let id = Id::generate();
    let episode = Episode {
        identity: Identity {
            id,
            ingested_at: ts(),
            namespace: Namespace::Agent("ops".to_string()),
            expired_at: None,
        },
        stats: stats(trust),
        content: format!("source {seed}"),
        role: Role::User,
        captured_at: ts(),
        agent_id: agent,
        session_id: None,
        content_hash: aionforge_domain::ids::ContentHash::of(&seed.to_le_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("insert episode");
    id
}

/// Assert a fact about a fresh subject entity; returns `(node, fact)`. The fact's own
/// `stats.trust` is a deliberately-wrong sentinel — the recompute must derive the baseline from
/// the source episode, never read this field back.
fn fact(store: &Store, statement: &str) -> (NodeId, Fact) {
    let subject = Entity {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts(),
            namespace: Namespace::Agent("ops".to_string()),
            expired_at: None,
        },
        stats: stats(0.5),
        canonical_name: format!("subject {statement}"),
        entity_type: "Concept".to_string(),
        aliases: vec![],
        description: None,
        embedding: None,
        embedder_model: None,
        attributes: None,
    };
    let subject_node = store.insert_entity(&subject).expect("entity");
    let fact = Fact {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts(),
            namespace: Namespace::Agent("ops".to_string()),
            expired_at: None,
        },
        stats: stats(0.123), // sentinel: never read back by the recompute
        subject_id: subject.identity.id,
        predicate: PREDICATE.to_string(),
        object: ObjectValue::Text("the team".to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: statement.to_string(),
        embedding: None,
        embedder_model: None,
        extraction: None,
        cooled_until: None,
    };
    let about = About {
        temporal: BiTemporal {
            valid_from: ts(),
            valid_to: None,
            ingested_at: ts(),
            expired_at: None,
        },
    };
    let node = store
        .assert_fact(&fact, subject_node, &about)
        .expect("assert fact");
    (node, fact)
}

/// Wire `Fact -DERIVED_FROM-> Episode`, the production-attribution edge.
fn derive(store: &Store, fact_id: &Id, episode_id: &Id) {
    let query = BoundQuery::new(
        "MATCH (f:Fact {id: $fact}), (e:Episode {id: $ep}) \
         INSERT (f)-[:DERIVED_FROM {derived_at: $at}]->(e)",
    )
    .bind_uuid("fact", fact_id)
    .expect("bind fact")
    .bind_uuid("ep", episode_id)
    .expect("bind ep")
    .bind_timestamp("at", &ts())
    .expect("bind at");
    store.execute(&query).expect("wire DERIVED_FROM");
}

/// Wire `Fact -ATTESTED_BY-> Agent`, the attestation edge the demotion trigger reads.
fn attest(store: &Store, fact_id: &Id, attester: &Id) {
    let query = BoundQuery::new(
        "MATCH (f:Fact {id: $fact}), (a:Agent {id: $att}) \
         INSERT (f)-[:ATTESTED_BY {attested_at: $at, signature: $sig, category: $cat}]->(a)",
    )
    .bind_uuid("fact", fact_id)
    .expect("bind fact")
    .bind_uuid("att", attester)
    .expect("bind att")
    .bind_timestamp("at", &ts())
    .expect("bind at")
    .bind_str("sig", "test-signature")
    .expect("bind sig")
    .bind_str("cat", PREDICATE)
    .expect("bind cat");
    store.execute(&query).expect("wire ATTESTED_BY");
}

fn agent_score(store: &Store, agent: &Id) -> Option<f64> {
    store
        .agent_by_id(agent)
        .expect("agent")
        .and_then(|a| a.trust_scores.0.get(PREDICATE).map(|c| c.score))
}

fn fact_trust(store: &Store, fact_node: NodeId) -> f64 {
    store
        .fact_by_node_id(fact_node)
        .expect("fact")
        .expect("present")
        .stats
        .trust
}

#[test]
fn a_contradiction_decays_the_producer_and_sinks_the_victim_fact() {
    let store = store();
    let agent = enroll(&store);
    let ep = episode(&store, agent, 0.8, 1); // baseline 0.8
    let (node, f) = fact(&store, "the team prefers graph databases");
    derive(&store, &f.identity.id, &ep);

    let scorer = scorer(Arc::clone(&store));
    let events = scorer.quarantine_decay(node, &ts()).expect("decay events");

    // One producing agent ⇒ one decay event against that agent in the fact's predicate category.
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].subject_id, agent);

    scorer.apply(&events).expect("apply");

    // The agent's reliability folds to one contradiction: alpha=1, beta=2, score=1/3.
    assert!((agent_score(&store, &agent).expect("scored") - 1.0 / 3.0).abs() < EPS);
    // The victim fact sinks to that reliability, derived from the 0.8 baseline (not the 0.123
    // sentinel it was stored with) min the decayed 1/3.
    assert!((fact_trust(&store, node) - 1.0 / 3.0).abs() < EPS);
}

#[test]
fn applying_the_same_decay_twice_is_idempotent() {
    let store = store();
    let agent = enroll(&store);
    let ep = episode(&store, agent, 0.8, 1);
    let (node, f) = fact(&store, "claim");
    derive(&store, &f.identity.id, &ep);

    let scorer = scorer(Arc::clone(&store));
    let events = scorer.quarantine_decay(node, &ts()).expect("events");
    scorer.apply(&events).expect("apply once");
    let once = agent_score(&store, &agent).expect("scored");
    // Re-applying the same content-addressed event records nothing new and refolds to the same.
    scorer.apply(&events).expect("apply twice");
    let twice = agent_score(&store, &agent).expect("scored");
    assert!(
        (once - twice).abs() < EPS,
        "a replayed decay does not double-count"
    );
    assert!((twice - 1.0 / 3.0).abs() < EPS);
}

#[test]
fn apply_counting_reports_zero_on_replay() {
    // The auto-sweep's true new-row count: the first application reports each event created;
    // a replay of the same content-addressed set reports zero. (That the replay still refolds
    // is pinned separately by `a_crash_between_record_and_refold_heals_on_the_re_scan`, where
    // the cache starts stale — here it was already correct, so it cannot tell.)
    let store = store();
    let agent = enroll(&store);
    let ep = episode(&store, agent, 0.8, 1);
    let (node, f) = fact(&store, "counted claim");
    derive(&store, &f.identity.id, &ep);

    let scorer = scorer(Arc::clone(&store));
    let events = scorer.quarantine_decay(node, &ts()).expect("events");
    assert_eq!(
        scorer.apply_counting(&events).expect("first apply"),
        1,
        "the first application records the new event"
    );
    assert_eq!(
        scorer.apply_counting(&events).expect("replay"),
        0,
        "a replay dedups every event and reports nothing new"
    );
    assert!((agent_score(&store, &agent).expect("scored") - 1.0 / 3.0).abs() < EPS);
}

#[test]
fn a_crash_between_record_and_refold_heals_on_the_re_scan() {
    // The heal `apply_counting` claims: the refold runs over every touched subject even when
    // nothing was created. Simulate the crash window by recording the event directly at the
    // store — log row committed, cache never refolded — then re-scan through the scorer. A
    // refold made conditional on `created > 0` leaves the cache stale and fails here.
    let store = store();
    let agent = enroll(&store);
    let ep = episode(&store, agent, 0.8, 1);
    let (node, f) = fact(&store, "healed claim");
    derive(&store, &f.identity.id, &ep);

    let scorer = scorer(Arc::clone(&store));
    let events = scorer.quarantine_decay(node, &ts()).expect("events");
    assert_eq!(events.len(), 1, "one producer ⇒ one decay event");
    assert!(
        store
            .record_reliability_update_created(&events[0])
            .expect("record without refold"),
        "the direct record is the first write"
    );
    assert_eq!(
        agent_score(&store, &agent),
        None,
        "the crash window: the event is recorded but the cache was never refolded"
    );

    // The re-scan dedups the row — zero created — yet still refolds the stale cache.
    assert_eq!(scorer.apply_counting(&events).expect("re-scan"), 0);
    assert!((agent_score(&store, &agent).expect("healed") - 1.0 / 3.0).abs() < EPS);
}

#[test]
fn an_agreement_credits_a_distinct_author_but_holds_the_fact_at_baseline() {
    let store = store();
    let producer = enroll(&store);
    let other = enroll(&store);
    let ep = episode(&store, producer, 0.8, 1);
    let (asserted_node, asserted) = fact(&store, "the asserted claim");
    derive(&store, &asserted.identity.id, &ep);
    // A corroborating fact authored by a DISTINCT agent.
    let other_ep = episode(&store, other, 0.8, 2);
    let (corroborating_node, corroborating) = fact(&store, "the corroborating claim");
    derive(&store, &corroborating.identity.id, &other_ep);

    let scorer = scorer(Arc::clone(&store));
    let events = scorer
        .agreement_gain(asserted_node, corroborating_node, &ts())
        .expect("gain events");
    assert_eq!(
        events.len(),
        1,
        "only the distinct-author producer is credited"
    );
    assert_eq!(events[0].subject_id, producer);

    scorer.apply(&events).expect("apply");
    // One agreement ⇒ alpha=1.25, score ≈ 0.5556.
    assert!((agent_score(&store, &producer).expect("scored") - 1.25 / 2.25).abs() < EPS);
    // But a gain never deflates a healthy fact: it holds at the 0.8 baseline (the gained 0.5556 is
    // above the prior, so it is inert).
    assert!((fact_trust(&store, asserted_node) - 0.8).abs() < EPS);
}

#[test]
fn an_agreement_never_credits_self_corroboration() {
    let store = store();
    let producer = enroll(&store);
    let ep = episode(&store, producer, 0.8, 1);
    let (asserted_node, asserted) = fact(&store, "the asserted claim");
    derive(&store, &asserted.identity.id, &ep);
    // The "corroborating" fact is authored by the SAME agent — the anti-farming guard drops it.
    let ep2 = episode(&store, producer, 0.8, 2);
    let (corroborating_node, corroborating) = fact(&store, "a self-restated claim");
    derive(&store, &corroborating.identity.id, &ep2);

    let scorer = scorer(Arc::clone(&store));
    let events = scorer
        .agreement_gain(asserted_node, corroborating_node, &ts())
        .expect("gain events");
    assert!(events.is_empty(), "an agent cannot corroborate itself");
}

#[test]
fn a_demotion_decays_each_attester() {
    let store = store();
    let ann = enroll(&store);
    let bo = enroll(&store);
    let (node, f) = fact(&store, "the demoted claim");
    attest(&store, &f.identity.id, &ann);
    attest(&store, &f.identity.id, &bo);

    let scorer = scorer(Arc::clone(&store));
    let events = scorer.demotion_decay(node, &ts()).expect("decay events");
    assert_eq!(events.len(), 2, "each distinct attester is decayed");

    scorer.apply(&events).expect("apply");
    assert!((agent_score(&store, &ann).expect("ann scored") - 1.0 / 3.0).abs() < EPS);
    assert!((agent_score(&store, &bo).expect("bo scored") - 1.0 / 3.0).abs() < EPS);
}

#[test]
fn an_unenrolled_producer_still_has_its_facts_re_derived() {
    let store = store();
    // A producer id that is NEVER enrolled — its episodes carry the agent_id, but no Agent node.
    let ghost = Id::generate();
    let ep = episode(&store, ghost, 0.8, 1); // baseline 0.8
    let (node, f) = fact(&store, "a claim by an unenrolled producer");
    derive(&store, &f.identity.id, &ep);

    let scorer = scorer(Arc::clone(&store));
    let events = scorer.quarantine_decay(node, &ts()).expect("decay events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].subject_id, ghost);

    scorer.apply(&events).expect("apply");

    // No Agent node exists, so there is no cached score to refresh.
    assert!(
        store.agent_by_id(&ghost).expect("read").is_none(),
        "the producer was never enrolled"
    );
    // The fact's trust still sinks: refold folds the producer's event log directly (alpha=1,
    // beta=2 ⇒ 1/3) and re-derives the fact from the 0.8 baseline min 1/3.
    assert!((fact_trust(&store, node) - 1.0 / 3.0).abs() < EPS);
}

#[test]
fn a_decayed_producer_sinks_a_co_produced_fact_a_neutral_co_producer_does_not_pin() {
    let store = store();
    let ann = enroll(&store);
    let bo = enroll(&store);
    // Fact f is co-produced by Ann (one episode) and Bo (another), baseline 0.8.
    let f_ep_ann = episode(&store, ann, 0.8, 1);
    let f_ep_bo = episode(&store, bo, 0.8, 2);
    let (f_node, f) = fact(&store, "the co-produced claim");
    derive(&store, &f.identity.id, &f_ep_ann);
    derive(&store, &f.identity.id, &f_ep_bo);
    // A SEPARATE fact g, produced only by Ann, is contradicted — decaying Ann (not Bo).
    let g_ep = episode(&store, ann, 0.8, 3);
    let (g_node, g) = fact(&store, "a separate claim ann was wrong about");
    derive(&store, &g.identity.id, &g_ep);

    let scorer = scorer(Arc::clone(&store));
    let events = scorer.quarantine_decay(g_node, &ts()).expect("events");
    scorer.apply(&events).expect("apply");

    // Refolding Ann recomputed f too (Ann co-produced it). f sinks to Ann's 1/3 — Bo, with no
    // reliability history, is inert and does NOT pin f to the 0.5 prior.
    assert!((agent_score(&store, &ann).expect("ann scored") - 1.0 / 3.0).abs() < EPS);
    assert!(agent_score(&store, &bo).is_none(), "bo took no decay");
    assert!((fact_trust(&store, f_node) - 1.0 / 3.0).abs() < EPS);
}
