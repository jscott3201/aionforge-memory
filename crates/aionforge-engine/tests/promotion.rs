//! End-to-end test for attestation and quorum promotion through the facade (06 §4, M4.T04).
//!
//! Exercises the real composition: the engine builds an Ed25519 [`AttestationGate`] and a
//! `Promoter` over the store's registered agent keys, so a fact promotes only when enough
//! distinct attesters sign it and the reliability-weighted posterior clears the threshold. The
//! posterior is bounded by attester quality; most cases tune the policy (a small quorum at a
//! lower threshold) so the math is easy to read, and one case exercises the shipped default
//! `(k = 3, threshold = 0.80)` directly to prove it is reachable by a strong consensus.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::edges::{About, SupersededBy};
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::agent::{Agent, AgentStatus, TrustCategory, TrustScores};
use aionforge_domain::nodes::forensic::PromotionStatus;
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::signing::attestation_payload;
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::value::ObjectValue;
use aionforge_engine::{
    AttestRequest, DemotionOutcome, EngineError, Memory, MemoryConfig, PromotionOutcome,
    PromotionPolicy,
};
use aionforge_store::{NodeId, Store, StoreConfig};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signer, SigningKey};

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

fn stats() -> Stats {
    Stats {
        importance: 0.5,
        trust: 0.8,
        last_access: ts(),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

/// Enroll an attester with `key`'s public key and a fixed reliability `score` in `category`.
fn enroll(store: &Store, key: &SigningKey, category: &str, score: f64) -> Id {
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
        identity: Identity {
            id,
            ingested_at: ts(),
            namespace: Namespace::Agent("ops".to_string()),
            expired_at: None,
        },
        public_key: BASE64.encode(key.verifying_key().to_bytes()),
        model_family: "test".to_string(),
        model_version: None,
        trust_scores: TrustScores(scores),
        status: AgentStatus::Active,
    };
    store.create_agent(&agent).expect("enroll");
    id
}

fn open_window() -> BiTemporal {
    BiTemporal {
        valid_from: ts(),
        valid_to: None,
        ingested_at: ts(),
        expired_at: None,
    }
}

/// Assert a team fact about a fresh subject entity; returns `(node id, fact id)`.
fn team_fact(store: &Store, statement: &str) -> (NodeId, Id) {
    let subject = Entity {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts(),
            namespace: Namespace::Team("acme".to_string()),
            expired_at: None,
        },
        stats: stats(),
        canonical_name: "graph databases".to_string(),
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
            namespace: Namespace::Team("acme".to_string()),
            expired_at: None,
        },
        stats: stats(),
        subject_id: subject.identity.id,
        predicate: "preferred_by".to_string(),
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
        .expect("assert team fact");
    (node, fact.identity.id)
}

/// A signed attestation request, timestamped at the real `now` so the gate admits it.
fn attest_request(fact_id: Id, attester: Id, key: &SigningKey, category: &str) -> AttestRequest {
    let attested_at = Timestamp::now();
    let payload = attestation_payload(&fact_id, &attester, &attested_at);
    AttestRequest {
        fact_id,
        attester_id: attester,
        attested_at,
        signature_b64: BASE64.encode(key.sign(&payload).to_bytes()),
        category: Some(category.to_string()),
    }
}

/// A tuned, enabled policy: a quorum of `k` and a reachable threshold.
fn policy(k: u64, threshold: f64) -> MemoryConfig {
    MemoryConfig {
        promotion: PromotionPolicy {
            enabled: true,
            default_k: k,
            default_threshold: threshold,
            default_category: "reliability".to_string(),
            ..PromotionPolicy::default()
        },
        ..MemoryConfig::default()
    }
}

#[test]
fn a_quorum_above_threshold_promotes_a_team_fact_to_global() {
    let store = migrated_store();
    let key_a = SigningKey::from_bytes(&[7u8; 32]);
    let key_b = SigningKey::from_bytes(&[8u8; 32]);
    let ada = enroll(&store, &key_a, "reliability", 0.95);
    let bo = enroll(&store, &key_b, "reliability", 0.95);
    let (_, fact_id) = team_fact(&store, "the team prefers graph databases");
    let memory = Memory::new(
        Arc::clone(&store),
        FakeEmbedder::new(),
        policy(2, 0.7),
        &ts(),
    )
    .expect("build promoting memory");

    // First attester: below the quorum of 2, so nothing promotes yet.
    let first = memory
        .attest(attest_request(fact_id, ada, &key_a, "reliability"))
        .expect("first attestation");
    assert!(first.recorded);
    assert_eq!(first.promoted, None, "one attester is below the quorum");

    // Second distinct attester: quorum met, posterior ~0.725 >= 0.7, so it promotes.
    let second = memory
        .attest(attest_request(fact_id, bo, &key_b, "reliability"))
        .expect("second attestation");
    assert!(second.recorded);
    let global_id = second.promoted.expect("the quorum promotes the fact");

    let global = store
        .fact_node_by_id(&global_id)
        .expect("probe")
        .and_then(|node| store.fact_by_node_id(node).expect("read"))
        .expect("global copy exists");
    assert_eq!(global.identity.namespace, Namespace::Global);
    let ledger = store
        .promotion_by_candidate(&fact_id)
        .expect("ledger")
        .expect("row");
    assert_eq!(ledger.status, PromotionStatus::Promoted);
}

#[test]
fn the_default_policy_promotes_under_a_strong_consensus() {
    let store = migrated_store();
    // Calibrated against the shipped default gates — fail loudly here if they ever change, since
    // the attester counts below bracket the 0.80 boundary precisely.
    let defaults = PromotionPolicy::default();
    assert_eq!((defaults.default_k, defaults.default_threshold), (3, 0.80));
    let config = policy(defaults.default_k, defaults.default_threshold);
    let (_, fact_id) = team_fact(&store, "the default policy can actually promote");
    let memory =
        Memory::new(Arc::clone(&store), FakeEmbedder::new(), config, &ts()).expect("memory");

    // Three near-perfect attesters meet the quorum of 3, but the bounded posterior (~0.794) is
    // just shy of 0.80 — both gates have to clear and the belief gate does not yet.
    for seed in 0..3u8 {
        let key = SigningKey::from_bytes(&[seed + 20; 32]);
        let attester = enroll(&store, &key, "reliability", 0.99);
        let receipt = memory
            .attest(attest_request(fact_id, attester, &key, "reliability"))
            .expect("attest");
        assert_eq!(
            receipt.promoted, None,
            "three attesters: quorum met, belief still short of 0.80"
        );
    }
    // A fourth pushes the posterior to ~0.827, clearing 0.80 — so the shipped default promotes.
    let key = SigningKey::from_bytes(&[24u8; 32]);
    let attester = enroll(&store, &key, "reliability", 0.99);
    let global_id = memory
        .attest(attest_request(fact_id, attester, &key, "reliability"))
        .expect("attest")
        .promoted
        .expect("a strong consensus clears the shipped default threshold");
    let global = store
        .fact_node_by_id(&global_id)
        .expect("probe")
        .and_then(|node| store.fact_by_node_id(node).expect("read"))
        .expect("global copy");
    assert_eq!(global.identity.namespace, Namespace::Global);
}

#[test]
fn a_superseded_fact_does_not_promote_even_with_a_quorum() {
    let store = migrated_store();
    let key_a = SigningKey::from_bytes(&[7u8; 32]);
    let key_b = SigningKey::from_bytes(&[8u8; 32]);
    let ada = enroll(&store, &key_a, "reliability", 0.95);
    let bo = enroll(&store, &key_b, "reliability", 0.95);
    let (team_node, fact_id) = team_fact(&store, "a claim that gets superseded");
    let memory = Memory::new(
        Arc::clone(&store),
        FakeEmbedder::new(),
        policy(2, 0.7),
        &ts(),
    )
    .expect("memory");

    // Supersede the fact before it is attested: it drops out of current_support_facts, so it has
    // lost standing even though its later attestations are perfectly valid.
    let (newer_node, _) = team_fact(&store, "the newer claim that replaces it");
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

    // A full quorum of high-reliability attesters signs it — enough to clear both gates if it
    // were current — but promotion is the dual of demotion and refuses a non-current fact.
    memory
        .attest(attest_request(fact_id, ada, &key_a, "reliability"))
        .expect("attest a");
    let second = memory
        .attest(attest_request(fact_id, bo, &key_b, "reliability"))
        .expect("attest b");
    assert_eq!(
        second.promoted, None,
        "a superseded fact does not promote on stale attestations"
    );
    assert!(matches!(
        memory.evaluate_promotion(&fact_id, &ts()).expect("eval"),
        PromotionOutcome::NotApplicable
    ));
    assert!(
        store
            .promotion_by_candidate(&fact_id)
            .expect("ledger")
            .is_none(),
        "no promotion ledger row for a non-current fact"
    );
}

#[test]
fn a_mediocre_swarm_never_clears_the_threshold() {
    let store = migrated_store();
    let (fact_node, fact_id) = team_fact(&store, "a contested claim");
    let memory = Memory::new(
        Arc::clone(&store),
        FakeEmbedder::new(),
        policy(2, 0.7),
        &ts(),
    )
    .expect("memory");

    // Eight attesters, each only r=0.6: the Beta posterior asymptotes to ~0.6, never to 0.7.
    for seed in 0..8u8 {
        let key = SigningKey::from_bytes(&[seed + 10; 32]);
        let attester = enroll(&store, &key, "reliability", 0.6);
        memory
            .attest(attest_request(fact_id, attester, &key, "reliability"))
            .expect("attest");
    }
    let outcome = memory
        .evaluate_promotion(&fact_id, &ts())
        .expect("evaluate");
    assert!(
        matches!(outcome, PromotionOutcome::NotYet { posterior, .. } if posterior < 0.7),
        "count alone cannot clear the threshold (sybil bound): {outcome:?}"
    );
    assert!(
        store
            .promotion_by_candidate(&fact_id)
            .expect("ledger")
            .is_none(),
        "nothing promoted, no ledger row written"
    );
    // The fact node is unpromoted, so no global copy exists.
    assert!(store.fact_by_node_id(fact_node).expect("read").is_some());
}

#[test]
fn a_repeat_attestation_by_one_agent_counts_once() {
    let store = migrated_store();
    let key = SigningKey::from_bytes(&[7u8; 32]);
    let ada = enroll(&store, &key, "reliability", 0.99);
    let (_, fact_id) = team_fact(&store, "single-agent claim");
    let memory = Memory::new(
        Arc::clone(&store),
        FakeEmbedder::new(),
        policy(2, 0.7),
        &ts(),
    )
    .expect("memory");

    memory
        .attest(attest_request(fact_id, ada, &key, "reliability"))
        .expect("attest once");
    let again = memory
        .attest(attest_request(fact_id, ada, &key, "reliability"))
        .expect("attest again");
    assert_eq!(
        again.promoted, None,
        "one agent attesting twice is still one vote, below the quorum of 2"
    );
}

#[test]
fn demotion_on_lost_support_quarantines_the_global_copy_and_leaves_the_original() {
    let store = migrated_store();
    let key_a = SigningKey::from_bytes(&[7u8; 32]);
    let key_b = SigningKey::from_bytes(&[8u8; 32]);
    let ada = enroll(&store, &key_a, "reliability", 0.95);
    let bo = enroll(&store, &key_b, "reliability", 0.95);
    let (team_node, fact_id) = team_fact(&store, "promote then lose support");
    let memory = Memory::new(
        Arc::clone(&store),
        FakeEmbedder::new(),
        policy(2, 0.7),
        &ts(),
    )
    .expect("memory");

    memory
        .attest(attest_request(fact_id, ada, &key_a, "reliability"))
        .expect("attest a");
    let global_id = memory
        .attest(attest_request(fact_id, bo, &key_b, "reliability"))
        .expect("attest b")
        .promoted
        .expect("promoted");
    let team_before = store
        .fact_by_node_id(team_node)
        .expect("read")
        .expect("team");

    // Lose support: supersede the team original, dropping it from current_support_facts.
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

    let outcome = memory.evaluate_demotion(&fact_id, &ts()).expect("demote");
    assert!(
        matches!(outcome, DemotionOutcome::Demoted { .. }),
        "{outcome:?}"
    );

    // The global copy is quarantined.
    let global = store
        .fact_node_by_id(&global_id)
        .expect("probe")
        .and_then(|node| store.fact_by_node_id(node).expect("read"))
        .expect("global");
    assert!(global.identity.expired_at.is_some());
    assert_eq!(global.status, FactStatus::Quarantined);
    let ledger = store.promotion_by_candidate(&fact_id).unwrap().unwrap();
    assert_eq!(ledger.status, PromotionStatus::Rejected);

    // The team original keeps the status the supersession set, but was not otherwise touched by
    // the demotion: its id, namespace, and content are unchanged.
    let team_after = store
        .fact_by_node_id(team_node)
        .expect("read")
        .expect("team");
    assert_eq!(team_after.identity.id, team_before.identity.id);
    assert_eq!(
        team_after.identity.namespace,
        team_before.identity.namespace
    );
    assert_eq!(team_after.statement, team_before.statement);
}

#[test]
fn promotion_off_records_nothing_and_evaluations_are_disabled() {
    let store = migrated_store();
    let key = SigningKey::from_bytes(&[7u8; 32]);
    let ada = enroll(&store, &key, "reliability", 0.95);
    let (_, fact_id) = team_fact(&store, "no promotion configured");
    let memory = Memory::new(
        Arc::clone(&store),
        FakeEmbedder::new(),
        MemoryConfig::default(),
        &ts(),
    )
    .expect("default memory");

    let receipt = memory
        .attest(attest_request(fact_id, ada, &key, "reliability"))
        .expect("attest");
    assert!(!receipt.recorded, "promotion off records no attestation");
    assert_eq!(receipt.promoted, None);
    assert!(matches!(
        memory.evaluate_promotion(&fact_id, &ts()).expect("eval"),
        PromotionOutcome::Disabled
    ));
    assert!(matches!(
        memory.evaluate_demotion(&fact_id, &ts()).expect("eval"),
        DemotionOutcome::Disabled
    ));
}

#[test]
fn a_wrong_key_attestation_is_refused_with_a_coarse_error() {
    let store = migrated_store();
    let enrolled = SigningKey::from_bytes(&[7u8; 32]);
    let attacker = SigningKey::from_bytes(&[9u8; 32]);
    let ada = enroll(&store, &enrolled, "reliability", 0.95);
    let (fact_node, fact_id) = team_fact(&store, "forge attempt");
    let memory = Memory::new(
        Arc::clone(&store),
        FakeEmbedder::new(),
        policy(2, 0.7),
        &ts(),
    )
    .expect("memory");

    // Signed by the attacker's key, not the one the store holds for `ada`.
    let error = memory
        .attest(attest_request(fact_id, ada, &attacker, "reliability"))
        .expect_err("a wrong-key attestation must be refused");
    assert!(matches!(error, EngineError::Promotion(_)));
    // No attestation edge was recorded.
    assert!(
        store
            .distinct_attesters(fact_node)
            .expect("attesters")
            .is_empty(),
        "a refused attestation records no edge"
    );
}

#[test]
fn a_quorum_of_one_is_a_config_error() {
    let store = migrated_store();
    let result = Memory::new(store, FakeEmbedder::new(), policy(1, 0.7), &ts());
    assert!(matches!(result, Err(EngineError::Config(_))));
}

#[test]
fn a_zero_skew_tolerance_with_promotion_on_is_a_config_error() {
    let store = migrated_store();
    let mut config = policy(2, 0.7);
    config.security.clock_skew_tolerance_ms = 0;
    let result = Memory::new(store, FakeEmbedder::new(), config, &ts());
    assert!(matches!(result, Err(EngineError::Config(_))));
}
