//! M5.T03 acceptance for the erase facade (05 §3): the one destructive path, end to
//! end through `Memory::erase`, behind both of its gates.
//!
//! - Erasure is **off by default**, independently of forgetting: an unconfigured
//!   memory answers `Disabled` and touches nothing, even for a live, erasable id.
//! - An owner erases its own ground end to end: the cascade dies, the purge audit
//!   lands in the owner's namespace with the **principal as actor**.
//! - The namespace authority covers the *whole* cascade: one spanned namespace the
//!   principal cannot write refuses the erasure entirely, leaving every member alive
//!   and writing no audit row.
//! - Under the default policy, global ground is not erasable by any plain principal —
//!   the same `NotDirectlyWritable` rule that confines capture confines erasure.

mod common;

use std::sync::Arc;

use aionforge_domain::authz::Principal;
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::embedding::Embedding;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::semantic::{Fact, FactStatus};
use aionforge_domain::time::Timestamp;
use aionforge_domain::value::ObjectValue;
use aionforge_engine::{ErasurePolicy, Memory, MemoryConfig, PointErase};
use aionforge_store::{BoundQuery, FactKey, MaterializedNote, Store, Value};
use common::{DIM, FakeEmbedder, migrated_store, ts};

fn fake_embedding() -> Embedding {
    Embedding::new(vec![1.0; DIM as usize]).expect("valid embedding")
}

fn now() -> Timestamp {
    "2026-06-06T12:00:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn memory(store: &Arc<Store>, erasure: ErasurePolicy) -> Memory<FakeEmbedder> {
    let config = MemoryConfig {
        erasure,
        ..MemoryConfig::default()
    };
    Memory::new(Arc::clone(store), FakeEmbedder::new(), config, &ts(0)).expect("memory")
}

fn erasure_on() -> ErasurePolicy {
    ErasurePolicy {
        enabled: true,
        ..ErasurePolicy::default()
    }
}

fn principal() -> Principal {
    Principal::agent(Id::from_content_hash(b"erase-acceptance-principal"))
}

fn own_namespace(principal: &Principal) -> Namespace {
    Namespace::Agent(principal.agent_id.to_string())
}

fn stats() -> Stats {
    Stats {
        importance: 0.9,
        trust: 0.9,
        last_access: now(),
        access_count_recent: 5,
        referenced_count: 2,
        surprise: 0.1,
        is_pinned: false,
    }
}

fn identity_in(namespace: Namespace) -> Identity {
    Identity {
        id: Id::generate(),
        ingested_at: now(),
        namespace,
        expired_at: None,
    }
}

fn episode(content: &str, namespace: Namespace) -> Episode {
    Episode {
        identity: identity_in(namespace),
        stats: stats(),
        content: content.to_string(),
        role: Role::User,
        captured_at: now(),
        agent_id: Id::from_content_hash(b"erase-acceptance-agent"),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: Some(fake_embedding()),
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    }
}

fn fact(statement: &str, namespace: Namespace) -> Fact {
    Fact {
        identity: identity_in(namespace),
        stats: stats(),
        subject_id: Id::from_content_hash(b"subject"),
        predicate: "tests".to_string(),
        object: ObjectValue::Text(statement.to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: statement.to_string(),
        embedding: Some(fake_embedding()),
        embedder_model: None,
        extraction: None,
        cooled_until: None,
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

fn is_live(store: &Store, id: &Id, label: &str) -> bool {
    store.memory_by_id(id, &[label]).expect("resolve").is_some()
}

#[test]
fn erasure_is_off_by_default_and_independent_of_forgetting() {
    let store = migrated_store();
    let memory = memory(&store, ErasurePolicy::default());
    let principal = principal();

    // Even a live memory in the principal's own namespace answers `Disabled`: the
    // off-switch is honest, never a fabricated "not found".
    let e = episode("alive under a disabled surface", own_namespace(&principal));
    store.insert_episode(&e).expect("insert");
    assert_eq!(
        memory
            .erase(&principal, &e.identity.id, &now())
            .expect("call"),
        PointErase::Disabled
    );
    assert!(is_live(&store, &e.identity.id, "Episode"));
}

#[test]
fn an_owner_erases_its_own_cascade_end_to_end() {
    let store = migrated_store();
    let memory = memory(&store, erasure_on());
    let principal = principal();
    let own_ns = own_namespace(&principal);

    let e = episode("the owner's source", own_ns.clone());
    let f = fact("the owner's derivative", own_ns.clone());
    store.insert_episode(&e).expect("insert");
    store.insert_fact(&f).expect("insert");
    derived_fact_edge(&store, &f.identity.id, &e.identity.id);

    let outcome = memory
        .erase(&principal, &e.identity.id, &now())
        .expect("erase");
    let PointErase::Erased(report) = outcome else {
        panic!("expected Erased, got {outcome:?}");
    };
    assert_eq!(report.seed, e.identity.id);
    assert_eq!(report.purged_nodes, 2);
    assert!(!is_live(&store, &e.identity.id, "Episode"));
    assert!(!is_live(&store, &f.identity.id, "Fact"));

    // The audit trail: the row lives in the owner's namespace and names the erasing
    // principal as actor — accountability for the one agent-driven destructive write.
    let rows = store
        .audit_by_kind(AuditKind::Purge, None, 10)
        .expect("audit")
        .events;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].identity.id, report.purge_audit_id);
    assert_eq!(rows[0].identity.namespace, own_ns);
    assert_eq!(rows[0].actor_id, principal.agent_id);
}

#[test]
fn a_cascade_spanning_an_unwritable_namespace_refuses_whole() {
    let store = migrated_store();
    let memory = memory(&store, erasure_on());
    let principal = principal();

    // The seed is the principal's own; a derivative lives in a team it does not
    // belong to. One unwritable namespace refuses the whole erasure.
    let e = episode("authorized seed", own_namespace(&principal));
    let f = fact(
        "derivative across the team boundary",
        Namespace::Team("atlas".to_string()),
    );
    store.insert_episode(&e).expect("insert");
    store.insert_fact(&f).expect("insert");
    derived_fact_edge(&store, &f.identity.id, &e.identity.id);

    let outcome = memory
        .erase(&principal, &e.identity.id, &now())
        .expect("call");
    assert_eq!(
        outcome,
        PointErase::Unauthorized {
            namespace: Namespace::Team("atlas".to_string())
        }
    );
    assert!(is_live(&store, &e.identity.id, "Episode"));
    assert!(is_live(&store, &f.identity.id, "Fact"));
    assert!(
        store
            .audit_by_kind(AuditKind::Purge, None, 10)
            .expect("audit")
            .events
            .is_empty(),
        "an unauthorized erase audits nothing"
    );
}

#[test]
fn global_ground_is_not_erasable_under_the_default_policy() {
    let store = migrated_store();
    let memory = memory(&store, erasure_on());
    let principal = principal();

    // `global` is never directly writable, so it is never directly erasable either —
    // promotion governance owns that ground, not any single principal.
    let f = fact("promoted shared ground", Namespace::Global);
    store.insert_fact(&f).expect("insert");

    let outcome = memory
        .erase(&principal, &f.identity.id, &now())
        .expect("call");
    assert_eq!(
        outcome,
        PointErase::Unauthorized {
            namespace: Namespace::Global
        }
    );
    assert!(is_live(&store, &f.identity.id, "Fact"));
}

fn note(content: &str, namespace: Namespace) -> Note {
    Note {
        identity: identity_in(namespace),
        stats: stats(),
        content: content.to_string(),
        context: None,
        keywords: Vec::new(),
        embedding: None,
        embedder_model: None,
        derived_from_episode: None,
    }
}

fn key_of(f: &Fact) -> FactKey {
    FactKey {
        subject_id: f.subject_id,
        predicate: f.predicate.clone(),
        object: f.object.clone(),
    }
}

fn distill_audit(seed: &[u8], namespace: Namespace, subject_id: Id) -> AuditEvent {
    AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(seed),
            ingested_at: now(),
            namespace,
            expired_at: None,
        },
        kind: AuditKind::Distill,
        subject_id,
        actor_id: Id::from_content_hash(b"distiller"),
        payload: serde_json::json!({"outcome": "written"}),
        signature: String::new(),
        occurred_at: now(),
    }
}

/// The spec acceptance chain (05 §3): a three-tier Episode -> Fact -> Note cascade
/// through the engine facade, with a multi-parent sibling note spared, the distill
/// audit rows surviving their subjects, and the dead rows physically reclaimed by
/// `Store::compact` — the documented durable boundary.
#[test]
fn the_episode_fact_note_chain_erases_and_compact_reclaims_it() {
    let store = migrated_store();
    let memory = memory(&store, erasure_on());
    let principal = principal();
    let own_ns = own_namespace(&principal);

    let e = episode("the source episode", own_ns.clone());
    let doomed_fact = fact("the derived claim", own_ns.clone());
    let surviving_fact = fact("the independent claim", own_ns.clone());
    store.insert_episode(&e).expect("insert");
    store.insert_fact(&doomed_fact).expect("insert");
    store.insert_fact(&surviving_fact).expect("insert");
    derived_fact_edge(&store, &doomed_fact.identity.id, &e.identity.id);

    // Two notes seeded via the note materializer (seed_notes_for_test): one grounded only in the
    // doomed fact (cascades with it), one grounded in both facts (spared by the
    // multi-parent survival rule — still standing on the independent claim).
    let cascading = note("summary of the derived claim", own_ns.clone());
    let spared = note("summary of both claims", own_ns.clone());
    let notes = vec![
        MaterializedNote {
            note: cascading.clone(),
            source_facts: vec![key_of(&doomed_fact)],
        },
        MaterializedNote {
            note: spared.clone(),
            source_facts: vec![key_of(&doomed_fact), key_of(&surviving_fact)],
        },
    ];
    store
        .seed_notes_for_test(&notes, &now())
        .expect("materialize notes");
    // The `Distill` tombstone audits are no longer wired by the note-write path: seed them
    // separately, each subject-bound to the note it produced so the by-subject decode finds it.
    store
        .commit_audit(&distill_audit(
            b"distill-cascading",
            own_ns.clone(),
            cascading.identity.id,
        ))
        .expect("seed cascading distill audit");
    store
        .commit_audit(&distill_audit(
            b"distill-spared",
            own_ns.clone(),
            spared.identity.id,
        ))
        .expect("seed spared distill audit");

    let outcome = memory
        .erase(&principal, &e.identity.id, &now())
        .expect("erase");
    let PointErase::Erased(report) = outcome else {
        panic!("expected Erased, got {outcome:?}");
    };
    assert_eq!(report.cascade_depth, 2, "episode -> fact -> note");
    assert_eq!(report.purged_nodes, 3);
    assert!(report.purged_node_ids.contains(&cascading.identity.id));
    assert_eq!(
        report.spared_multiparent,
        vec![spared.identity.id],
        "the multi-parent note is spared and named"
    );
    assert!(!is_live(&store, &e.identity.id, "Episode"));
    assert!(!is_live(&store, &doomed_fact.identity.id, "Fact"));
    assert!(!is_live(&store, &cascading.identity.id, "Note"));
    assert!(is_live(&store, &surviving_fact.identity.id, "Fact"));
    assert!(is_live(&store, &spared.identity.id, "Note"));

    // Audit rows are never closure members: both distill audits survive the purge of
    // the note they produced (the `Distill` tombstone is subject-bound, not edge-wired,
    // so the deletion never reaches it), and the purge row joins them.
    assert_eq!(
        store
            .audit_by_kind(AuditKind::Distill, None, 10)
            .expect("audit")
            .events
            .len(),
        2
    );
    assert_eq!(
        store
            .audit_by_kind(AuditKind::Purge, None, 10)
            .expect("audit")
            .events
            .len(),
        1
    );

    // The durable boundary: compaction physically reclaims the dead rows the report's
    // residual_retention honestly said were still resident.
    assert!(report.residual_retention.live_until_compact);
    let compacted = store.compact().expect("compact");
    assert!(
        compacted.reclaimed_nodes >= 3,
        "the cascade's rows are physically gone: {compacted:?}"
    );
}
