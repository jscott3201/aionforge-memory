//! Integration tests for reliability-decay demotion over a real store (06 §4–§5, M4.T05 PR-D).
//!
//! Reliability demotion is the state-disjoint complement of the structural lost-support demotion:
//! it un-promotes a global copy whose team original is **still current** but whose attesters'
//! reliability has decayed below the quorum bar. These tests promote a team fact on strong
//! attesters, decay those attesters through the real scorer (the refold-first contract), and assert
//! the promoted copy is quarantined — while the structural path defers and a superseded fact routes
//! the other way. The promotion gate's signature check lives in `attest`; here we wire the
//! `ATTESTED_BY` edges directly and drive `evaluate_promotion`, so no signing harness is needed.

use std::sync::Arc;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::edges::{About, SupersededBy};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::agent::{Agent, AgentStatus, TrustCategory, TrustScores};
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::PromotionStatus;
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::value::ObjectValue;
use aionforge_store::{BoundQuery, NodeId, Store, StoreConfig};
use aionforge_trust::{
    AttestationGate, DemotionOutcome, Ed25519Verifier, Promoter, PromotionOutcome, PromotionPolicy,
    ReliabilityPolicy, ReliabilityScorer, StoreKeyResolver, SystemWallClock,
};

/// The attestation/predicate category every helper shares, so the promotion posterior and the
/// scorer's decay both read and write the same `TrustCategory` slot.
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

/// A tuned, enabled promoter: a quorum of `k = 2` at a reachable `threshold = 0.70`. Two attesters
/// at reliability 0.95 clear it (posterior 0.725); once decayed to ≈0.333 they fall under (0.417).
/// The gate is built but unused — `evaluate_promotion`/`evaluate_reliability_demotion` never verify
/// signatures — so any resolver/clock suffices.
fn promoter(store: Arc<Store>, enabled: bool) -> Promoter {
    let gate = AttestationGate::new(
        Ed25519Verifier,
        Arc::new(StoreKeyResolver::new(Arc::clone(&store))),
        Arc::new(SystemWallClock),
        5_000,
    );
    let policy = PromotionPolicy {
        enabled,
        default_k: 2,
        default_threshold: 0.70,
        ..PromotionPolicy::default()
    };
    Promoter::new(store, gate, policy)
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

fn open_window() -> BiTemporal {
    BiTemporal {
        valid_from: ts(),
        valid_to: None,
        ingested_at: ts(),
        expired_at: None,
    }
}

/// Enroll an attester with reliability `score` in the shared category; returns its id. The cached
/// score is what the promotion posterior reads before any decay refolds it from the event log.
fn enroll(store: &Store, score: f64) -> Id {
    let id = Id::generate();
    let mut scores = std::collections::BTreeMap::new();
    scores.insert(
        PREDICATE.to_string(),
        TrustCategory {
            alpha: 1.0,
            beta: 1.0,
            score,
        },
    );
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
        trust_scores: TrustScores(scores),
        status: AgentStatus::Active,
    };
    store.create_agent(&agent).expect("enroll");
    id
}

/// Assert a fact about a fresh subject entity in `namespace`; returns `(node, fact)`.
fn fact(store: &Store, namespace: Namespace, statement: &str) -> (NodeId, Fact) {
    let subject = Entity {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts(),
            namespace: namespace.clone(),
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
            namespace,
            expired_at: None,
        },
        stats: stats(0.8),
        subject_id: subject.identity.id,
        predicate: PREDICATE.to_string(),
        object: ObjectValue::Text("the team".to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: statement.to_string(),
        embedding: None,
        embedder_model: None,
        extraction: None,
    };
    let node = store
        .assert_fact(
            &fact,
            subject_node,
            &About {
                temporal: open_window(),
            },
        )
        .expect("assert fact");
    (node, fact)
}

/// A team-namespace fact (the promotion candidate); returns `(node, fact id)`.
fn team_fact(store: &Store, statement: &str) -> (NodeId, Id) {
    let (node, f) = fact(store, Namespace::Team("acme".to_string()), statement);
    (node, f.identity.id)
}

/// Insert a raw episode captured by `agent`; returns its id.
fn episode(store: &Store, agent: Id, seed: u128) -> Id {
    let id = Id::generate();
    let episode = Episode {
        identity: Identity {
            id,
            ingested_at: ts(),
            namespace: Namespace::Agent("ops".to_string()),
            expired_at: None,
        },
        stats: stats(0.8),
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

/// Wire `Fact -DERIVED_FROM-> Episode`, the production-attribution edge the decay path reads.
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

/// Wire `Fact -ATTESTED_BY-> Agent` in the shared category, the quorum the demotion gate recomputes.
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

/// Decay `agent`'s reliability to one failure (≈0.333) by contradicting a throwaway fact it
/// produced, then refolding — the realistic stand-in for the engine sweep's refold-first step.
fn decay_attester(store: &Store, scorer: &ReliabilityScorer, agent: Id, seed: u128) {
    let ep = episode(store, agent, seed);
    let (node, f) = fact(
        store,
        Namespace::Agent("ops".to_string()),
        &format!("junk {seed}"),
    );
    derive(store, &f.identity.id, &ep);
    let events = scorer.quarantine_decay(node, &ts()).expect("decay events");
    scorer.apply(&events).expect("apply decay");
}

/// Promote a fresh team fact on two strong attesters; returns `(team node, fact id, global id, ann,
/// bo)`. The attesters are enrolled at 0.95 so the posterior (0.725) clears the 0.70 bar.
fn promote(store: &Store, promoter: &Promoter, statement: &str) -> (NodeId, Id, Id, Id, Id) {
    let ann = enroll(store, 0.95);
    let bo = enroll(store, 0.95);
    let (team_node, fact_id) = team_fact(store, statement);
    attest(store, &fact_id, &ann);
    attest(store, &fact_id, &bo);
    let outcome = promoter
        .evaluate_promotion(&fact_id, &ts())
        .expect("evaluate promotion");
    let PromotionOutcome::Promoted { global_id, .. } = outcome else {
        panic!("expected promotion, got {outcome:?}");
    };
    (team_node, fact_id, global_id, ann, bo)
}

fn quarantined(store: &Store, global_id: &Id) -> bool {
    store
        .fact_node_by_id(global_id)
        .expect("probe")
        .and_then(|node| store.fact_by_node_id(node).expect("read"))
        .is_some_and(|f| f.status == FactStatus::Quarantined && f.identity.expired_at.is_some())
}

#[test]
fn reliability_decay_demotes_a_promoted_fact_and_quarantines_the_global_copy() {
    let store = store();
    let promoter = promoter(Arc::clone(&store), true);
    let scorer = scorer(Arc::clone(&store));
    let (team_node, fact_id, global_id, ann, bo) =
        promote(&store, &promoter, "the team prefers graph databases");

    // While the attesters are still reliable, the STRUCTURAL path sees a current, supported fact
    // and does nothing — the two demotion triggers are disjoint on this state.
    assert!(
        matches!(
            promoter
                .evaluate_demotion(&fact_id, &ts())
                .expect("structural"),
            DemotionOutcome::NoChange
        ),
        "a current fact is not structurally demoted"
    );

    // Decay both attesters below the bar (refold-first), then the reliability path fires.
    decay_attester(&store, &scorer, ann, 10);
    decay_attester(&store, &scorer, bo, 11);
    let outcome = promoter
        .evaluate_reliability_demotion(&fact_id, &ts())
        .expect("reliability demote");
    assert!(
        matches!(outcome, DemotionOutcome::Demoted { global_id: g } if g == global_id),
        "{outcome:?}"
    );

    // The global copy is quarantined and expired; the ledger flips to Rejected.
    assert!(quarantined(&store, &global_id), "global copy quarantined");
    let ledger = store
        .promotion_by_candidate(&fact_id)
        .expect("ledger")
        .expect("present");
    assert_eq!(ledger.status, PromotionStatus::Rejected);

    // The team original is left untouched: still current, still Active.
    let team = store
        .fact_by_node_id(team_node)
        .expect("read")
        .expect("team");
    assert_eq!(team.status, FactStatus::Active);
    assert!(team.identity.expired_at.is_none());
}

#[test]
fn a_still_reliable_promotion_is_not_reliability_demoted() {
    let store = store();
    let promoter = promoter(Arc::clone(&store), true);
    let (_, fact_id, global_id, _, _) = promote(&store, &promoter, "a well-attested claim");

    // No decay: the posterior still clears the bar, so reliability demotion leaves it promoted.
    let outcome = promoter
        .evaluate_reliability_demotion(&fact_id, &ts())
        .expect("eval");
    assert!(matches!(outcome, DemotionOutcome::NoChange), "{outcome:?}");
    assert!(
        !quarantined(&store, &global_id),
        "a still-reliable global copy stays active"
    );
}

#[test]
fn a_reliability_demotion_is_idempotent() {
    let store = store();
    let promoter = promoter(Arc::clone(&store), true);
    let scorer = scorer(Arc::clone(&store));
    let (_, fact_id, global_id, ann, bo) = promote(&store, &promoter, "promote then decay");
    decay_attester(&store, &scorer, ann, 20);
    decay_attester(&store, &scorer, bo, 21);

    let first = promoter
        .evaluate_reliability_demotion(&fact_id, &ts())
        .expect("first");
    assert!(
        matches!(first, DemotionOutcome::Demoted { .. }),
        "{first:?}"
    );

    // The ledger is now Rejected, so a second evaluation is a clean no-op (not a double demotion).
    let second = promoter
        .evaluate_reliability_demotion(&fact_id, &ts())
        .expect("second");
    assert!(
        matches!(second, DemotionOutcome::NoChange),
        "a re-evaluation after demotion no-ops: {second:?}"
    );
    assert!(quarantined(&store, &global_id), "still quarantined once");
}

#[test]
fn a_team_fact_that_lost_support_defers_to_the_structural_path() {
    let store = store();
    let promoter = promoter(Arc::clone(&store), true);
    let scorer = scorer(Arc::clone(&store));
    let (team_node, fact_id, global_id, ann, bo) =
        promote(&store, &promoter, "promote, decay, then lose support");
    // The attesters HAVE decayed — reliability demotion would otherwise fire.
    decay_attester(&store, &scorer, ann, 30);
    decay_attester(&store, &scorer, bo, 31);

    // But the team original then loses support (superseded), dropping out of the current set.
    let (newer_node, _) = team_fact(&store, "a newer claim supersedes it");
    store
        .supersede_fact(
            team_node,
            newer_node,
            &SupersededBy {
                reason: "newer".to_string(),
                temporal: open_window(),
            },
        )
        .expect("supersede");

    // Reliability demotion defers: a non-current fact is the structural path's responsibility.
    let outcome = promoter
        .evaluate_reliability_demotion(&fact_id, &ts())
        .expect("eval");
    assert!(
        matches!(outcome, DemotionOutcome::NoChange),
        "a fact that lost support is not reliability-demoted: {outcome:?}"
    );
    assert!(
        !quarantined(&store, &global_id),
        "reliability demotion did not touch the global copy"
    );
}

#[test]
fn reliability_demotion_is_disabled_when_promotion_is_off() {
    let store = store();
    let (_, fact_id) = team_fact(&store, "no promotion configured");
    let promoter = promoter(Arc::clone(&store), false);
    assert!(matches!(
        promoter
            .evaluate_reliability_demotion(&fact_id, &ts())
            .expect("eval"),
        DemotionOutcome::Disabled
    ));
}
