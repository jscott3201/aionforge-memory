//! Shared fixtures for the reliability-sweep suites: deterministic embedders, a migrated
//! store and a reliability-on memory, producer-backed victim facts, and emitter-shaped
//! quarantine audit rows. Split out so each suite stays within the file-size cap; each
//! test binary uses a subset, so dead-code is allowed here rather than per-item.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use aionforge_consolidate::CONTRADICTION_QUARANTINE_REASON;
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::edges::About;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::agent::{Agent, AgentStatus, TrustCategory, TrustScores};
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::value::ObjectValue;
use aionforge_engine::{Memory, MemoryConfig};
use aionforge_store::{BoundQuery, NodeId, Store, StoreConfig};
use aionforge_trust::ReliabilityPolicy;

pub const PREDICATE: &str = "preferred_by";
pub const DIM: u32 = 12;
pub const EPS: f64 = 1e-9;

#[derive(Clone)]
pub struct FakeEmbedder {
    model: EmbedderModel,
}
impl FakeEmbedder {
    pub fn new() -> Self {
        Self {
            model: EmbedderModel {
                family: "fake".to_string(),
                version: "1".to_string(),
                dimension: DIM,
            },
        }
    }
}
#[derive(Debug)]
pub struct NeverError;
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
            .map(|_| {
                let mut components = vec![0.0f32; DIM as usize];
                components[0] = 1.0;
                Embedding::new(components).expect("valid")
            })
            .collect();
        async move { Ok(out) }
    }
    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

/// A one-hot embedder for the real-pipeline test: distinct surfaces are orthogonal (never
/// cluster), identical surfaces coreference — mirrors the consolidate detection fixture so the
/// "up"/"down" contradiction survives resolution as two distinct facts.
#[derive(Clone)]
pub struct AxisEmbedder {
    model: EmbedderModel,
}
impl AxisEmbedder {
    pub fn new() -> Self {
        Self {
            model: EmbedderModel {
                family: "axis-fake".to_string(),
                version: "1".to_string(),
                dimension: DIM,
            },
        }
    }
}
impl Embedder for AxisEmbedder {
    type Error = NeverError;
    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let out: Vec<Embedding> = inputs
            .iter()
            .map(|text| {
                let axis = text.trim().bytes().map(usize::from).sum::<usize>() % (DIM as usize);
                let mut components = vec![0.0f32; DIM as usize];
                components[axis] = 1.0;
                Embedding::new(components).expect("valid")
            })
            .collect();
        async move { Ok(out) }
    }
    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

/// A timestamp at the given minute, so rows order deterministically under `(occurred_at, id)`.
pub fn ts(minute: u32) -> Timestamp {
    format!("2026-06-09T09:{minute:02}:00-05:00[America/Chicago]")
        .parse()
        .expect("valid zoned datetime")
}

pub fn migrated_store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: DIM,
    })
    .expect("open store");
    store.migrate(&ts(0)).expect("migrate");
    Arc::new(store)
}

/// A memory with reliability scoring on (the sweep's only switch); promotion stays off.
pub fn memory(store: &Arc<Store>) -> Memory<FakeEmbedder> {
    let config = MemoryConfig {
        reliability: ReliabilityPolicy {
            enabled: true,
            ..ReliabilityPolicy::default()
        },
        ..MemoryConfig::default()
    };
    Memory::new(Arc::clone(store), FakeEmbedder::new(), config, &ts(0)).expect("memory")
}

pub fn stats(trust: f64) -> Stats {
    Stats {
        importance: 0.5,
        trust,
        last_access: ts(0),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

pub fn enroll(store: &Store) -> Id {
    let id = Id::generate();
    let mut scores = BTreeMap::new();
    scores.insert(
        PREDICATE.to_string(),
        TrustCategory {
            alpha: 1.0,
            beta: 1.0,
            score: 0.95,
        },
    );
    let agent = Agent {
        identity: Identity {
            id,
            ingested_at: ts(0),
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

/// Assert a fact about a fresh subject entity in `namespace`; returns `(node, fact id)`.
pub fn fact(store: &Store, namespace: &Namespace, statement: &str) -> (NodeId, Id) {
    let subject = Entity {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts(0),
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
            ingested_at: ts(0),
            namespace: namespace.clone(),
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
        cooled_until: None,
    };
    let node = store
        .assert_fact(
            &fact,
            subject_node,
            &About {
                temporal: BiTemporal {
                    valid_from: ts(0),
                    valid_to: None,
                    ingested_at: ts(0),
                    expired_at: None,
                },
            },
        )
        .expect("assert fact");
    (node, fact.identity.id)
}

/// Insert a raw episode captured by `agent` and wire `fact_id -DERIVED_FROM-> episode`, making
/// `agent` a producer of the fact.
pub fn produce(store: &Store, fact_id: &Id, agent: Id, seed: u128) {
    let episode_id = Id::generate();
    let episode = Episode {
        identity: Identity {
            id: episode_id,
            ingested_at: ts(0),
            namespace: Namespace::Agent("ops".to_string()),
            expired_at: None,
        },
        stats: stats(0.8),
        content: format!("source {seed}"),
        role: Role::User,
        captured_at: ts(0),
        agent_id: agent,
        session_id: None,
        content_hash: ContentHash::of(&seed.to_le_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("insert episode");
    let query = BoundQuery::new(
        "MATCH (f:Fact {id: $fact}), (e:Episode {id: $ep}) \
         INSERT (f)-[:DERIVED_FROM {derived_at: $at}]->(e)",
    )
    .bind_uuid("fact", fact_id)
    .expect("bind fact")
    .bind_uuid("ep", episode_id)
    .expect("bind ep")
    .bind_timestamp("at", &ts(0))
    .expect("bind at");
    store.execute(&query).expect("wire DERIVED_FROM");
}

/// A producer-backed victim fact in `namespace`: asserted, derived from one episode by a fresh
/// enrolled agent. Returns `(fact id, producer agent id)`.
pub fn victim(store: &Store, namespace: &Namespace, seed: u128) -> (Id, Id) {
    let agent = enroll(store);
    let (_, fact_id) = fact(store, namespace, &format!("victim {seed}"));
    produce(store, &fact_id, agent, seed);
    (fact_id, agent)
}

/// An emitter-shaped contradiction-quarantine audit row (the consolidation pass's shape: the
/// victim is the subject, a pass actor distinct from it, the victim/survivor payload, and the
/// shared reason const). Committed through the store's audit funnel like the real co-commit.
pub fn commit_contradiction_quarantine(
    store: &Store,
    victim_id: &Id,
    namespace: &Namespace,
    survivor_object: &str,
    minute: u32,
) {
    let at = ts(minute);
    let event = AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(
                format!("test-quarantine|{victim_id}|{survivor_object}").as_bytes(),
            ),
            ingested_at: at.clone(),
            namespace: namespace.clone(),
            expired_at: None,
        },
        kind: AuditKind::Quarantine,
        subject_id: *victim_id,
        actor_id: Id::from_content_hash(b"pass-actor"),
        payload: serde_json::json!({
            "predicate": PREDICATE,
            "victim_object": "down",
            "victim_trust": 0.5,
            "survivor_object": survivor_object,
            "survivor_trust": 0.9,
            "reason": CONTRADICTION_QUARANTINE_REASON,
        }),
        signature: String::new(),
        occurred_at: at,
    };
    store.commit_audit(&event).expect("commit quarantine");
}

/// A governance demotion-quarantine row (the promoter's shape: subject == actor == the global
/// copy, the demote payload) — must be skipped by the D1 sweep.
pub fn commit_governance_quarantine(store: &Store, minute: u32) {
    let global = Id::from_content_hash(b"global-copy");
    let at = ts(minute);
    let event = AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(format!("test-governance|{minute}").as_bytes()),
            ingested_at: at.clone(),
            namespace: Namespace::System,
            expired_at: None,
        },
        kind: AuditKind::Quarantine,
        subject_id: global,
        actor_id: global,
        payload: serde_json::json!({
            "candidate_fact_id": "candidate",
            "promoted_fact_id": global.to_string(),
            "reason": "lost_support",
            "posterior": 0.4,
            "k": 2,
        }),
        signature: String::new(),
        occurred_at: at,
    };
    store
        .commit_audit(&event)
        .expect("commit governance quarantine");
}

pub fn agent_score_in(store: &Store, agent: &Id, category: &str) -> Option<f64> {
    store
        .agent_by_id(agent)
        .expect("agent")
        .and_then(|a| a.trust_scores.0.get(category).map(|c| c.score))
}

pub fn agent_score(store: &Store, agent: &Id) -> Option<f64> {
    agent_score_in(store, agent, PREDICATE)
}

pub fn reliability_event_count(store: &Store) -> usize {
    store
        .audit_by_kind(AuditKind::ReliabilityUpdate, None, 200)
        .expect("read")
        .events
        .len()
}
