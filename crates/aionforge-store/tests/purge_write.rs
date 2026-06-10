//! Store-level tests for the hard-purge write (05 §3, M5.T03): one transaction over the
//! whole closure, the audit co-committed and gated on a real deletion, attestation
//! severed while shared entities and prior audit rows survive, every retrieval surface
//! forgetting instantly, and the WAL round-trip.

use std::path::PathBuf;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::edges::AttestedBy;
use aionforge_domain::embedding::Embedding;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::agent::{Agent, AgentStatus, TrustScores};
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::time::Timestamp;
use aionforge_domain::value::ObjectValue;
use aionforge_store::{
    BoundQuery, CascadeCaps, ClosureOutcome, NodeId, PurgeClosure, PurgeWrite, SearchKind, Store,
    StoreConfig, Value,
};

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

fn now() -> Timestamp {
    ts("2026-06-06T12:00:00-05:00[America/Chicago]")
}

fn store() -> Store {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store
        .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
        .expect("migrate store");
    store
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aionforge-purge-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn caps() -> CascadeCaps {
    CascadeCaps {
        max_depth: 16,
        max_nodes: 200,
    }
}

fn identity() -> Identity {
    Identity {
        id: Id::generate(),
        ingested_at: ts("2026-06-01T09:00:00-05:00[America/Chicago]"),
        namespace: Namespace::Global,
        expired_at: None,
    }
}

fn stats() -> Stats {
    Stats {
        importance: 0.5,
        trust: 0.5,
        last_access: ts("2026-06-01T09:00:00-05:00[America/Chicago]"),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

fn episode(content: &str) -> Episode {
    Episode {
        identity: identity(),
        stats: stats(),
        content: content.to_string(),
        role: Role::User,
        captured_at: now(),
        agent_id: Id::from_content_hash(b"purge-agent"),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: Some(Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("embedding")),
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    }
}

fn fact(statement: &str) -> Fact {
    Fact {
        identity: identity(),
        stats: stats(),
        subject_id: Id::from_content_hash(b"subject"),
        predicate: "tests".to_string(),
        object: ObjectValue::Text(statement.to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: statement.to_string(),
        embedding: None,
        embedder_model: None,
        extraction: None,
    }
}

fn purge_audit(seed: Id, seed_tag: &str) -> AuditEvent {
    AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(seed_tag.as_bytes()),
            ingested_at: now(),
            namespace: Namespace::Global,
            expired_at: None,
        },
        kind: AuditKind::Purge,
        subject_id: seed,
        actor_id: Id::from_content_hash(b"purge-agent"),
        payload: serde_json::json!({"reason": "right_to_erasure", "cascade_count": 0}),
        signature: String::new(),
        occurred_at: now(),
    }
}

fn derived_fact_edge(store: &Store, fact_id: &Id, episode_id: &Id) {
    let bound = BoundQuery::new(
        "MATCH (a:Fact {id: $from}), (b:Episode {id: $to}) \
         INSERT (a)-[:DERIVED_FROM {derived_at: $ts}]->(b)",
    )
    .bind_uuid("from", fact_id)
    .unwrap()
    .bind_uuid("to", episode_id)
    .unwrap()
    .bind("ts", Value::ZonedDateTime(Box::new(now())))
    .unwrap();
    store.execute(&bound).expect("insert DERIVED_FROM edge");
}

fn mentions_edge(store: &Store, episode_id: &Id, entity_id: &Id) {
    let bound = BoundQuery::new(
        "MATCH (a:Episode {id: $from}), (b:Entity {id: $to}) \
         INSERT (a)-[:MENTIONS {valid_from: $ts, ingested_at: $ts}]->(b)",
    )
    .bind_uuid("from", episode_id)
    .unwrap()
    .bind_uuid("to", entity_id)
    .unwrap()
    .bind("ts", Value::ZonedDateTime(Box::new(now())))
    .unwrap();
    store.execute(&bound).expect("insert MENTIONS edge");
}

fn closure_of(store: &Store, seed: NodeId) -> PurgeClosure {
    match store.derived_from_closure(seed, &caps()).expect("walk") {
        ClosureOutcome::Computed(closure) => closure,
        other => panic!("expected a computed closure, got {other:?}"),
    }
}

fn is_live(store: &Store, id: &Id, label: &str) -> bool {
    store.memory_by_id(id, &[label]).expect("resolve").is_some()
}

fn audit_count(store: &Store, kind: AuditKind) -> usize {
    store
        .audit_by_kind(kind, None, 200)
        .expect("audit page")
        .events
        .len()
}

#[test]
fn a_purge_destroys_the_closure_audits_once_and_replays_converge() {
    let store = store();
    let e = episode("the erased source");
    let f = fact("its derivative");
    let e_node = store.insert_episode(&e).expect("insert");
    store.insert_fact(&f).expect("insert");
    derived_fact_edge(&store, &f.identity.id, &e.identity.id);

    let closure = closure_of(&store, e_node);
    assert_eq!(closure.nodes.len(), 2);

    let outcome = store
        .hard_purge(&closure.nodes, &purge_audit(e.identity.id, "purge-1"))
        .expect("purge");
    assert_eq!(
        outcome,
        PurgeWrite::Applied {
            deleted_nodes: 2,
            deleted_edges: 1,
        },
        "two nodes and the one derivation edge between them"
    );
    assert!(!is_live(&store, &e.identity.id, "Episode"));
    assert!(!is_live(&store, &f.identity.id, "Fact"));

    // The purge trail: one row, addressed to the seed, reachable by subject even
    // though the subject node no longer exists.
    assert_eq!(audit_count(&store, AuditKind::Purge), 1);
    let row = &store
        .audit_by_kind(AuditKind::Purge, None, 10)
        .expect("audit")
        .events[0];
    assert_eq!(row.subject_id, e.identity.id);

    // A replay of the applied purge is a no-op with no second row.
    let replay = store
        .hard_purge(&closure.nodes, &purge_audit(e.identity.id, "purge-replay"))
        .expect("replay");
    assert_eq!(replay, PurgeWrite::Noop);
    assert_eq!(audit_count(&store, AuditKind::Purge), 1, "single audit row");
}

#[test]
fn attestation_is_severed_while_entities_and_prior_audits_survive() {
    let store = store();

    // An attested fact: the attestation edge is exactly what soft-forget refuses to
    // touch and hard purge removes.
    let attested = fact("attested and erased");
    let attested_node = store.insert_fact(&attested).expect("insert");
    let attester_id = Id::generate();
    let agent = Agent {
        identity: Identity {
            id: attester_id,
            ingested_at: now(),
            namespace: Namespace::Agent("ada".to_string()),
            expired_at: None,
        },
        public_key: "cHVibGljLWtleQ==".to_string(),
        model_family: "test".to_string(),
        model_version: None,
        trust_scores: TrustScores::default(),
        status: AgentStatus::Active,
    };
    let agent_node = store.create_agent(&agent).expect("enroll agent");
    let attest_audit = AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(b"attest-audit"),
            ingested_at: now(),
            namespace: Namespace::Agent("ada".to_string()),
            expired_at: None,
        },
        kind: AuditKind::Attest,
        subject_id: attester_id,
        actor_id: attester_id,
        payload: serde_json::json!({"outcome": "accepted"}),
        signature: String::new(),
        occurred_at: now(),
    };
    store
        .attest_fact(
            attested_node,
            agent_node,
            &AttestedBy {
                attested_at: now(),
                signature: "sig".to_string(),
                category: None,
            },
            &attest_audit,
        )
        .expect("attest");

    // A shared entity, mentioned by the erased episode AND linked to nothing doomed.
    let e = episode("mentions the shared entity");
    let entity = Entity {
        identity: identity(),
        stats: stats(),
        canonical_name: "selene".to_string(),
        entity_type: "Project".to_string(),
        aliases: Vec::new(),
        description: None,
        embedding: None,
        embedder_model: None,
        attributes: None,
    };
    let e_node = store.insert_episode(&e).expect("insert");
    store.insert_entity(&entity).expect("insert");
    mentions_edge(&store, &e.identity.id, &entity.identity.id);

    // Purge the attested fact (a singleton closure).
    let fact_closure = closure_of(&store, attested_node);
    store
        .hard_purge(
            &fact_closure.nodes,
            &purge_audit(attested.identity.id, "purge-fact"),
        )
        .expect("purge fact");
    assert!(!is_live(&store, &attested.identity.id, "Fact"));
    assert!(
        !store
            .has_adjacent_edge(agent_node, &[AttestedBy::LABEL])
            .expect("probe"),
        "the attestation edge died with its fact"
    );
    assert_eq!(
        audit_count(&store, AuditKind::Attest),
        1,
        "the prior attest audit row survives the purge of its subject"
    );

    // Purge the episode: the mentioned entity survives, and the MENTIONS edge to it —
    // an edge whose far endpoint lives — is both severed and counted.
    let episode_closure = closure_of(&store, e_node);
    let episode_outcome = store
        .hard_purge(
            &episode_closure.nodes,
            &purge_audit(e.identity.id, "purge-episode"),
        )
        .expect("purge episode");
    assert_eq!(
        episode_outcome,
        PurgeWrite::Applied {
            deleted_nodes: 1,
            deleted_edges: 1,
        },
        "the edge to the surviving entity is in the severed count"
    );
    assert!(!is_live(&store, &e.identity.id, "Episode"));
    assert!(
        is_live(&store, &entity.identity.id, "Entity"),
        "a shared entity is never a closure member"
    );
}

#[test]
fn every_retrieval_surface_forgets_instantly() {
    let store = store();
    let e = episode("the zanzibar protocol notes");
    let e_node = store.insert_episode(&e).expect("insert");

    let query_embedding = Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("embedding");
    assert!(
        store
            .text_search(SearchKind::Episode, "zanzibar", 5)
            .expect("text search")
            .iter()
            .any(|hit| hit.node == e_node),
        "text index sees the episode before the purge"
    );
    assert!(
        store
            .vector_search_exact(SearchKind::Episode, &query_embedding, 5)
            .expect("vector search")
            .iter()
            .any(|hit| hit.node == e_node),
        "vector index sees the episode before the purge"
    );

    let closure = closure_of(&store, e_node);
    store
        .hard_purge(&closure.nodes, &purge_audit(e.identity.id, "purge-search"))
        .expect("purge");

    assert!(
        store
            .text_search(SearchKind::Episode, "zanzibar", 5)
            .expect("text search")
            .is_empty(),
        "the text index forgot in the same write"
    );
    assert!(
        !store
            .vector_search_exact(SearchKind::Episode, &query_embedding, 5)
            .expect("vector search")
            .iter()
            .any(|hit| hit.node == e_node),
        "the vector index is search-unreachable in the same write"
    );
}

#[test]
fn a_partially_dead_closure_deletes_the_survivors() {
    let store = store();
    let e = episode("partially purged source");
    let f = fact("survives the first pass");
    let e_node = store.insert_episode(&e).expect("insert");
    let f_node = store.insert_fact(&f).expect("insert");
    derived_fact_edge(&store, &f.identity.id, &e.identity.id);

    // First purge the fact alone (its own singleton closure).
    store
        .hard_purge(&[f_node], &purge_audit(f.identity.id, "purge-first"))
        .expect("purge fact");
    assert!(!is_live(&store, &f.identity.id, "Fact"));

    // The stale two-member closure now has one dead member: the survivor still falls.
    let outcome = store
        .hard_purge(
            &[e_node, f_node],
            &purge_audit(e.identity.id, "purge-second"),
        )
        .expect("purge rest");
    assert_eq!(
        outcome,
        PurgeWrite::Applied {
            deleted_nodes: 1,
            deleted_edges: 0,
        },
        "only the live member is deleted and counted"
    );
    assert!(!is_live(&store, &e.identity.id, "Episode"));
}

#[test]
fn the_wal_round_trips_a_purge() {
    let dir = temp_dir("wal");
    let config = StoreConfig {
        embedding_dimension: 4,
    };
    let e = episode("purged before the crash");
    let keeper = episode("still here after recovery");
    {
        let store = Store::open_persistent_migrated(&dir, config, &now()).expect("open persistent");
        let e_node = store.insert_episode(&e).expect("insert");
        store.insert_episode(&keeper).expect("insert");
        let closure = closure_of(&store, e_node);
        store
            .hard_purge(&closure.nodes, &purge_audit(e.identity.id, "purge-wal"))
            .expect("purge");
        drop(store);
    }

    let recovered = Store::recover(&dir, config).expect("recover");
    assert!(
        !is_live(&recovered, &e.identity.id, "Episode"),
        "a purged memory stays purged across recovery"
    );
    assert!(
        is_live(&recovered, &keeper.identity.id, "Episode"),
        "the surviving memory recovers"
    );
    assert_eq!(audit_count(&recovered, AuditKind::Purge), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A deterministic stand-in for the substrate audit signer (the real one is Ed25519 in
/// aionforge-trust): object-safe, content-derived output, no crypto.
#[derive(Debug)]
struct FakeSigner;
impl aionforge_domain::verify::AuditEventSigner for FakeSigner {
    fn sign(&self, event: &AuditEvent) -> String {
        format!("fake-sig|{}", event.identity.id)
    }
}

#[test]
fn an_installed_signer_stamps_the_purge_row_at_commit() {
    let store = store();
    store
        .install_audit_signer(std::sync::Arc::new(FakeSigner))
        .expect("install signer");
    let f = fact("signed and purged");
    let node = store.insert_fact(&f).expect("insert");

    let event = purge_audit(f.identity.id, "signed-purge");
    store.hard_purge(&[node], &event).expect("purge");
    let row = &store
        .audit_by_kind(AuditKind::Purge, None, 10)
        .expect("audit")
        .events[0];
    assert_eq!(
        row.signature,
        format!("fake-sig|{}", event.identity.id),
        "the blank purge event was stamped inside the commit by the installed signer"
    );
}

#[test]
fn identical_content_reinserted_after_a_purge_resolves_and_searches() {
    let store = store();
    let first = episode("reinserted after a purge");
    let node = store.insert_episode(&first).expect("insert");
    store
        .hard_purge(&[node], &purge_audit(first.identity.id, "purge-reinsert"))
        .expect("purge");

    // The same content under a fresh id: the purge left no residue that blocks it.
    let again = Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: now(),
            namespace: Namespace::Global,
            expired_at: None,
        },
        ..first.clone()
    };
    store.insert_episode(&again).expect("re-insert");
    assert!(is_live(&store, &again.identity.id, "Episode"));
    assert!(
        !store
            .text_search(SearchKind::Episode, "reinserted", 5)
            .expect("search")
            .is_empty(),
        "the re-inserted content is searchable through the maintained text index"
    );
}
