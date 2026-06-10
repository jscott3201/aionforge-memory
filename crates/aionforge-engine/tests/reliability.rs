//! End-to-end tests for the reliability-scoring facade on `Memory` (06 §5, M4.T05 PR-E).
//!
//! These exercise the engine wiring, not the scorer internals (those are unit- and integration-
//! tested in `aionforge-trust`): the `Option<ReliabilityScorer>` off-switch, the host-driven decay
//! and agreement wrappers, and — the headline — the refold-first `sweep_reliability_demotions`,
//! which refreshes a promoted fact's attesters from the committed event log before the
//! reliability-demotion gate reads their reliability. Facts are promoted through
//! `evaluate_promotion` over directly-wired `ATTESTED_BY` edges, so no signing harness is needed.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::edges::About;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::agent::{Agent, AgentStatus, TrustCategory, TrustScores};
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::PromotionStatus;
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::value::ObjectValue;
use aionforge_engine::{DemotionOutcome, Memory, MemoryConfig, PromotionOutcome, PromotionPolicy};
use aionforge_store::{BoundQuery, NodeId, Store, StoreConfig};
use aionforge_trust::{ReliabilityPolicy, ReliabilityScorer};

const PREDICATE: &str = "preferred_by";
const EPS: f64 = 1e-9;

#[derive(Clone)]
struct FakeEmbedder {
    model: EmbedderModel,
}
impl FakeEmbedder {
    fn new() -> Self {
        Self {
            model: EmbedderModel {
                family: "fake".to_string(),
                version: "1".to_string(),
                dimension: 4,
            },
        }
    }
}
#[derive(Debug)]
struct NeverError;
impl std::fmt::Display for NeverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unreachable")
    }
}
impl std::error::Error for NeverError {}
impl Embedder for FakeEmbedder {
    type Error = NeverError;
    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let out = inputs
            .iter()
            .map(|_| Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("valid"))
            .collect();
        async move { Ok(out) }
    }
    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

fn ts() -> Timestamp {
    "2026-06-08T09:30:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn migrated_store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store.migrate(&ts()).expect("migrate");
    Arc::new(store)
}

/// A memory with trust scoring on; promotion is tuned to a reachable `k = 2 @ 0.70` so two strong
/// attesters promote and a decayed pair falls under the bar. Promotion is off unless `promote`.
fn memory(store: &Arc<Store>, promote: bool) -> Memory<FakeEmbedder> {
    let config = MemoryConfig {
        promotion: PromotionPolicy {
            enabled: promote,
            default_k: 2,
            default_threshold: 0.70,
            ..PromotionPolicy::default()
        },
        reliability: ReliabilityPolicy {
            enabled: true,
            ..ReliabilityPolicy::default()
        },
        ..MemoryConfig::default()
    };
    Memory::new(Arc::clone(store), FakeEmbedder::new(), config, &ts()).expect("memory")
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

/// Enroll an agent with reliability `score` in the shared category; returns its id.
fn enroll(store: &Store, score: f64) -> Id {
    let id = Id::generate();
    let mut scores = BTreeMap::new();
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

fn team_fact(store: &Store, statement: &str) -> Id {
    fact(store, Namespace::Team("acme".to_string()), statement)
        .1
        .identity
        .id
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
        content_hash: ContentHash::of(&seed.to_le_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("insert episode");
    id
}

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

/// A fact produced by `agent` (an episode it captured, wired by `DERIVED_FROM`); returns its id.
fn produced_fact(store: &Store, agent: Id, seed: u128) -> Id {
    let ep = episode(store, agent, seed);
    let (_, f) = fact(
        store,
        Namespace::Agent("ops".to_string()),
        &format!("junk {seed}"),
    );
    derive(store, &f.identity.id, &ep);
    f.identity.id
}

fn agent_score(store: &Store, agent: &Id) -> Option<f64> {
    store
        .agent_by_id(agent)
        .expect("agent")
        .and_then(|a| a.trust_scores.0.get(PREDICATE).map(|c| c.score))
}

fn quarantined(store: &Store, global_id: &Id) -> bool {
    store
        .fact_node_by_id(global_id)
        .expect("probe")
        .and_then(|node| store.fact_by_node_id(node).expect("read"))
        .is_some_and(|f| f.status == FactStatus::Quarantined && f.identity.expired_at.is_some())
}

/// Stage a producer decay for `agent` into the reliability event log **without** folding it into the
/// agent's trust cache — the realistic "events recorded, not yet refolded" state the refold-first
/// sweep must resolve. A standalone scorer builds the content-addressed events; the store records
/// them raw, so `Agent.trust_scores` keeps its enrolled value until something refolds.
fn stage_decay(store: &Arc<Store>, agent: Id, seed: u128) {
    let victim = store
        .fact_node_by_id(&produced_fact(store, agent, seed))
        .expect("probe")
        .expect("present");
    let scorer = ReliabilityScorer::new(
        Arc::clone(store),
        ReliabilityPolicy {
            enabled: true,
            ..ReliabilityPolicy::default()
        },
    );
    for event in &scorer.quarantine_decay(victim, &ts()).expect("build decay") {
        store
            .record_reliability_update(event)
            .expect("record raw event");
    }
}

#[test]
fn the_sweep_demotes_a_promotion_after_its_attesters_are_decayed() {
    // The end-to-end facade path: promote, decay the attesters through the host-driven wrapper, then
    // sweep. (The wrapper folds on apply, so the sweep's own refold is a no-op here; the dedicated
    // refold-first proof is the next test, which leaves the cache stale.)
    let store = migrated_store();
    let memory = memory(&store, true);
    let ada = enroll(&store, 0.95);
    let bo = enroll(&store, 0.95);
    let fact_id = team_fact(&store, "promote then let the attesters decay");
    attest(&store, &fact_id, &ada);
    attest(&store, &fact_id, &bo);

    // Promote: two 0.95 attesters clear the 0.70 bar (posterior 0.725).
    let PromotionOutcome::Promoted { global_id, .. } =
        memory.evaluate_promotion(&fact_id, &ts()).expect("promote")
    else {
        panic!("expected promotion");
    };

    // Decay both attesters through the host-driven facade: each produced a junk fact, contradicted.
    assert_eq!(
        memory
            .record_reliability_decay(&produced_fact(&store, ada, 10), &ts())
            .expect("decay ada"),
        1
    );
    assert_eq!(
        memory
            .record_reliability_decay(&produced_fact(&store, bo, 11), &ts())
            .expect("decay bo"),
        1
    );

    // Both attesters now read ≈0.333, so the gate fires: posterior ≈0.417 < 0.70.
    let outcomes = memory
        .sweep_reliability_demotions(&[fact_id], &ts())
        .expect("sweep");
    assert!(
        matches!(outcomes.as_slice(), [DemotionOutcome::Demoted { global_id: g }] if *g == global_id),
        "{outcomes:?}"
    );
    assert!(quarantined(&store, &global_id), "global copy quarantined");
    assert_eq!(
        store
            .promotion_by_candidate(&fact_id)
            .expect("ledger")
            .expect("present")
            .status,
        PromotionStatus::Rejected
    );
}

#[test]
fn the_sweep_refolds_a_stale_attester_cache_before_demoting() {
    // The load-bearing refold-first proof. The attesters' decay events are staged into the log but
    // NOT folded, so their caches still read the enrolled 0.95 — at which the posterior (0.725)
    // clears the 0.70 bar and a non-refolding sweep would report NoChange. The sweep must refold the
    // attesters from the committed log (→ 0.333, posterior 0.417) for the demotion to fire at all.
    let store = migrated_store();
    let memory = memory(&store, true);
    let ada = enroll(&store, 0.95);
    let bo = enroll(&store, 0.95);
    let fact_id = team_fact(&store, "promote then stage a decay without folding");
    attest(&store, &fact_id, &ada);
    attest(&store, &fact_id, &bo);
    let PromotionOutcome::Promoted { global_id, .. } =
        memory.evaluate_promotion(&fact_id, &ts()).expect("promote")
    else {
        panic!("expected promotion");
    };

    // Stage the decays raw — events in the log, caches untouched.
    stage_decay(&store, ada, 12);
    stage_decay(&store, bo, 13);
    assert!(
        (agent_score(&store, &ada).expect("ada") - 0.95).abs() < EPS
            && (agent_score(&store, &bo).expect("bo") - 0.95).abs() < EPS,
        "the caches are stale at 0.95 before the sweep"
    );

    // The sweep's own refold is the only thing that can lower these caches; without it the gate
    // would read 0.95 and not demote.
    let outcomes = memory
        .sweep_reliability_demotions(&[fact_id], &ts())
        .expect("sweep");
    assert!(
        matches!(outcomes.as_slice(), [DemotionOutcome::Demoted { .. }]),
        "the sweep refolded the stale caches and demoted: {outcomes:?}"
    );
    assert!(quarantined(&store, &global_id), "global copy quarantined");
    assert!(
        (agent_score(&store, &ada).expect("ada") - 1.0 / 3.0).abs() < EPS,
        "the sweep's refold persisted the decayed score"
    );
}

#[test]
fn the_sweep_is_disabled_without_the_promoter_half() {
    // Reliability ON, promotion OFF: the scorer can refold but there is no promoter to evaluate the
    // demotion, so every candidate reports Disabled — the promoter-off arm of the both-on guard.
    let store = migrated_store();
    let memory = memory(&store, false);
    let fact_id = team_fact(&store, "no promoter to demote through");
    assert!(matches!(
        memory
            .sweep_reliability_demotions(&[fact_id], &ts())
            .expect("sweep")
            .as_slice(),
        [DemotionOutcome::Disabled]
    ));
}

#[test]
fn the_sweep_reports_no_change_for_an_unpromoted_candidate() {
    // Both halves on: an unknown id resolves to no node, and a real-but-never-promoted fact has no
    // Promoted ledger — both report NoChange rather than demoting anything.
    let store = migrated_store();
    let memory = memory(&store, true);
    let unpromoted = team_fact(&store, "asserted but never promoted");
    let unknown = Id::generate();
    let outcomes = memory
        .sweep_reliability_demotions(&[unpromoted, unknown], &ts())
        .expect("sweep");
    assert!(
        matches!(
            outcomes.as_slice(),
            [DemotionOutcome::NoChange, DemotionOutcome::NoChange]
        ),
        "{outcomes:?}"
    );
}

#[test]
fn the_reliability_facade_is_inert_when_trust_scoring_is_off() {
    let store = migrated_store();
    // Promotion on, reliability OFF (default) — the scorer half is absent.
    let config = MemoryConfig {
        promotion: PromotionPolicy {
            enabled: true,
            default_k: 2,
            default_threshold: 0.70,
            ..PromotionPolicy::default()
        },
        ..MemoryConfig::default()
    };
    let memory =
        Memory::new(Arc::clone(&store), FakeEmbedder::new(), config, &ts()).expect("memory");
    let some = Id::generate();

    assert!(memory.refold_reliability(&[some]).is_ok());
    assert_eq!(
        memory.record_reliability_decay(&some, &ts()).expect("d1"),
        0
    );
    assert_eq!(
        memory
            .record_reliability_demotion(&some, &ts())
            .expect("d2"),
        0
    );
    assert_eq!(
        memory
            .record_reliability_agreement(&some, &Id::generate(), &ts())
            .expect("g1"),
        0
    );
    // Even with promotion on, the sweep needs the scorer half — every candidate is Disabled.
    assert!(matches!(
        memory
            .sweep_reliability_demotions(&[some], &ts())
            .expect("sweep")
            .as_slice(),
        [DemotionOutcome::Disabled]
    ));
}

#[test]
fn record_reliability_decay_folds_the_producer_and_is_idempotent() {
    let store = migrated_store();
    let memory = memory(&store, false);
    let agent = enroll(&store, 0.95);
    let victim = produced_fact(&store, agent, 20);

    assert_eq!(
        memory
            .record_reliability_decay(&victim, &ts())
            .expect("once"),
        1
    );
    let once = agent_score(&store, &agent).expect("scored");
    assert!((once - 1.0 / 3.0).abs() < EPS, "one contradiction ⇒ 1/3");

    // A replay rebuilds the same content-addressed event; apply dedups, so the score is unchanged.
    assert_eq!(
        memory
            .record_reliability_decay(&victim, &ts())
            .expect("twice"),
        1
    );
    let twice = agent_score(&store, &agent).expect("scored");
    assert!(
        (once - twice).abs() < EPS,
        "a replayed decay does not double-count"
    );
}

#[test]
fn record_reliability_demotion_folds_an_attester_and_is_idempotent() {
    // The D2 wrapper's positive path: a demoted fact's attester takes a w_attest_invalid decay,
    // folded into its category score, idempotent on replay.
    let store = migrated_store();
    let memory = memory(&store, false);
    let attester = enroll(&store, 0.95);
    let fact_id = team_fact(&store, "a demoted fact whose attester pays");
    attest(&store, &fact_id, &attester);

    assert_eq!(
        memory
            .record_reliability_demotion(&fact_id, &ts())
            .expect("once"),
        1
    );
    let once = agent_score(&store, &attester).expect("scored");
    assert!((once - 1.0 / 3.0).abs() < EPS, "one attest-invalid ⇒ 1/3");

    // A replay rebuilds the same content-addressed event; apply dedups, so the score is unchanged.
    assert_eq!(
        memory
            .record_reliability_demotion(&fact_id, &ts())
            .expect("twice"),
        1
    );
    let twice = agent_score(&store, &attester).expect("scored");
    assert!(
        (once - twice).abs() < EPS,
        "a replayed demotion decay does not double-count"
    );
}

#[test]
fn record_reliability_agreement_credits_only_a_distinct_author() {
    let store = migrated_store();
    let memory = memory(&store, false);
    let producer = enroll(&store, 0.5);
    let other = enroll(&store, 0.5);

    let asserted = produced_fact(&store, producer, 30);
    let corroborating = produced_fact(&store, other, 31);
    assert_eq!(
        memory
            .record_reliability_agreement(&asserted, &corroborating, &ts())
            .expect("distinct author"),
        1,
        "a distinct author corroboration credits the producer"
    );

    // A self-restatement is dropped by the scorer's distinct-author guard.
    let self_restated = produced_fact(&store, producer, 32);
    assert_eq!(
        memory
            .record_reliability_agreement(&asserted, &self_restated, &ts())
            .expect("self"),
        0,
        "an agent cannot corroborate itself"
    );
}
