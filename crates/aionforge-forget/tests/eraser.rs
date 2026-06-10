//! Orchestrator acceptance for right-to-erasure (05 §3, M5.T03): one seed id becomes
//! one irreversible, audited, fully-reported cascade; every refusal is typed and
//! decided before the write; the forgetter's protections do not gate the purge; and
//! the cross-namespace promoted copy is named, never silently followed or forgotten.

use std::sync::Arc;

use aionforge_domain::authz::{
    AuthorizationError, Authorizer, DefaultAuthorizer, Principal, VisibleSet,
};
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::edges::AttestedBy;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::agent::{Agent, AgentStatus, TrustScores};
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind, ProvenanceRecord};
use aionforge_domain::nodes::semantic::{Fact, FactStatus};
use aionforge_domain::time::Timestamp;
use aionforge_domain::value::ObjectValue;
use aionforge_forget::{Eraser, ErasurePolicy, PointErase};
use aionforge_store::{BoundQuery, Store, StoreConfig, Value};

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

fn now() -> Timestamp {
    ts("2026-06-06T12:00:00-05:00[America/Chicago]")
}

fn store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store
        .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
        .expect("migrate store");
    Arc::new(store)
}

fn eraser(store: &Arc<Store>) -> Eraser {
    Eraser::new(
        Arc::clone(store),
        ErasurePolicy {
            enabled: true,
            ..ErasurePolicy::default()
        },
    )
}

/// Write-permissive test authority. The fixtures place memories across global and
/// team namespaces the default policy refuses, and most tests here exercise the
/// cascade, not the gate — authorization has its own test below, on the real
/// [`DefaultAuthorizer`].
#[derive(Debug)]
struct PermitAll;

impl Authorizer for PermitAll {
    fn authorize_write(&self, _: &Principal, _: &Namespace) -> Result<(), AuthorizationError> {
        Ok(())
    }

    fn visible_namespaces(&self, principal: &Principal) -> VisibleSet {
        DefaultAuthorizer.visible_namespaces(principal)
    }
}

fn principal() -> Principal {
    Principal::agent(Id::from_content_hash(b"erase-principal"))
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
        ingested_at: ts("2026-06-01T09:00:00-05:00[America/Chicago]"),
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
        agent_id: Id::from_content_hash(b"erase-agent"),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: None,
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
        embedding: None,
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

fn promoted_edge(store: &Store, team_fact: &Id, global_fact: &Id) {
    let bound = BoundQuery::new(
        "MATCH (a:Fact {id: $from}), (b:Fact {id: $to}) \
         INSERT (a)-[:PROMOTED_TO {valid_from: $ts, ingested_at: $ts}]->(b)",
    )
    .bind_uuid("from", team_fact)
    .unwrap()
    .bind_uuid("to", global_fact)
    .unwrap()
    .bind("ts", Value::ZonedDateTime(Box::new(now())))
    .unwrap();
    store.execute(&bound).expect("insert PROMOTED_TO edge");
}

fn is_live(store: &Store, id: &Id, label: &str) -> bool {
    store.memory_by_id(id, &[label]).expect("resolve").is_some()
}

#[test]
fn an_erase_cascades_audits_and_reports_in_full() {
    let store = store();
    let eraser = eraser(&store);
    let owner_ns = Namespace::Agent("erasure-owner".to_string());

    // A captured episode (with its provenance record) and a derived fact.
    let e = episode("the erased source", owner_ns.clone());
    let provenance = ProvenanceRecord {
        identity: identity_in(owner_ns.clone()),
        subject_id: e.identity.id,
        writer_agent_id: Id::from_content_hash(b"erase-agent"),
        signature: "sig".to_string(),
        source_episode_ids: Vec::new(),
        model_family: "test".to_string(),
        model_version: None,
        trust_at_write: 0.5,
    };
    let capture_audit = AuditEvent {
        identity: identity_in(owner_ns.clone()),
        kind: AuditKind::Capture,
        subject_id: e.identity.id,
        actor_id: Id::from_content_hash(b"erase-agent"),
        payload: serde_json::json!({"reason": "test"}),
        signature: String::new(),
        occurred_at: now(),
    };
    store
        .commit_capture(&e, &provenance, &capture_audit)
        .expect("capture");
    let f = fact("its derivative", owner_ns.clone());
    store.insert_fact(&f).expect("insert");
    derived_fact_edge(&store, &f.identity.id, &e.identity.id);

    let outcome = eraser
        .erase(&principal(), &PermitAll, &e.identity.id, &now())
        .expect("erase");
    let PointErase::Erased(report) = outcome else {
        panic!("expected Erased, got {outcome:?}");
    };
    assert_eq!(report.seed, e.identity.id);
    assert_eq!(report.purged_nodes, 3, "episode, fact, provenance record");
    assert_eq!(report.purged_node_ids.len(), 3);
    assert!(report.purged_node_ids.contains(&provenance.identity.id));
    assert_eq!(report.cascade_depth, 1);
    assert_eq!(report.purged_provenance, 1);
    assert!(report.spared_multiparent.is_empty());
    assert!(report.promoted_shadows.is_empty());
    assert!(report.residual_retention.live_until_compact);
    assert!(report.residual_retention.wal_archive_until_snapshot);
    assert!(!is_live(&store, &e.identity.id, "Episode"));
    assert!(!is_live(&store, &f.identity.id, "Fact"));

    // The purge trail: one row in the memory's own namespace, scalar payload, and the
    // report's audit id is the row's id.
    let rows = store
        .audit_by_kind(AuditKind::Purge, None, 10)
        .expect("audit")
        .events;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].identity.id, report.purge_audit_id);
    assert_eq!(rows[0].subject_id, e.identity.id);
    assert_eq!(rows[0].identity.namespace, owner_ns);
    assert_eq!(
        rows[0].actor_id,
        principal().agent_id,
        "the erasing principal is the audit actor"
    );
    assert_eq!(rows[0].payload["reason"], "right_to_erasure");
    assert_eq!(rows[0].payload["cascade_count"], 3);

    // A repeated erase of the same id finds nothing: gone is gone.
    assert_eq!(
        eraser
            .erase(&principal(), &PermitAll, &e.identity.id, &now())
            .expect("replay"),
        PointErase::NotFound
    );
    assert_eq!(
        store
            .audit_by_kind(AuditKind::Purge, None, 10)
            .expect("audit")
            .events
            .len(),
        1,
        "a replay audits nothing"
    );
}

#[test]
fn erase_succeeds_where_every_forgetting_protection_would_refuse() {
    let store = store();
    let eraser = eraser(&store);

    // A pinned AND attested fact: the forgetter spares it twice over; the eraser
    // consults neither gate — erasure is the escalation those gates defer to.
    let protected = fact("pinned, attested, and erased anyway", Namespace::Global);
    let protected_node = store.insert_fact(&protected).expect("insert");
    let pin_audit = AuditEvent {
        identity: identity_in(Namespace::Global),
        kind: AuditKind::Pin,
        subject_id: protected.identity.id,
        actor_id: Id::from_content_hash(b"erase-agent"),
        payload: serde_json::json!({"reason": "manual_pin"}),
        signature: String::new(),
        occurred_at: now(),
    };
    store.set_pinned(protected_node, &pin_audit).expect("pin");
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
        identity: identity_in(Namespace::Agent("ada".to_string())),
        kind: AuditKind::Attest,
        subject_id: attester_id,
        actor_id: attester_id,
        payload: serde_json::json!({"outcome": "accepted"}),
        signature: String::new(),
        occurred_at: now(),
    };
    store
        .attest_fact(
            protected_node,
            agent_node,
            &AttestedBy {
                attested_at: now(),
                signature: "sig".to_string(),
                category: None,
            },
            &attest_audit,
        )
        .expect("attest");

    let outcome = eraser
        .erase(&principal(), &PermitAll, &protected.identity.id, &now())
        .expect("erase");
    assert!(
        matches!(outcome, PointErase::Erased(_)),
        "no forgetting protection gates the purge: {outcome:?}"
    );
    assert!(!is_live(&store, &protected.identity.id, "Fact"));
}

#[test]
fn refusals_are_typed_and_decided_before_any_write() {
    let store = store();

    // Unknown id.
    assert_eq!(
        eraser(&store)
            .erase(&principal(), &PermitAll, &Id::generate(), &now())
            .expect("call"),
        PointErase::NotFound
    );

    // An over-cap cascade refuses whole, leaving everything alive.
    let tight = Eraser::new(
        Arc::clone(&store),
        ErasurePolicy {
            enabled: true,
            max_cascade_nodes: 2,
            ..ErasurePolicy::default()
        },
    );
    let e = episode("over-cap source", Namespace::Global);
    let f1 = fact("first derivative", Namespace::Global);
    let f2 = fact("second derivative", Namespace::Global);
    store.insert_episode(&e).expect("insert");
    store.insert_fact(&f1).expect("insert");
    store.insert_fact(&f2).expect("insert");
    derived_fact_edge(&store, &f1.identity.id, &e.identity.id);
    derived_fact_edge(&store, &f2.identity.id, &e.identity.id);

    let outcome = tight
        .erase(&principal(), &PermitAll, &e.identity.id, &now())
        .expect("call");
    assert!(
        matches!(
            outcome,
            PointErase::CascadeTooLarge {
                nodes_observed: 3,
                ..
            }
        ),
        "refused whole: {outcome:?}"
    );
    for (id, label) in [
        (&e.identity.id, "Episode"),
        (&f1.identity.id, "Fact"),
        (&f2.identity.id, "Fact"),
    ] {
        assert!(is_live(&store, id, label), "nothing was touched");
    }
    assert_eq!(
        store
            .audit_by_kind(AuditKind::Purge, None, 10)
            .expect("audit")
            .events
            .len(),
        0,
        "a refusal audits nothing"
    );
}

#[test]
fn survivors_and_promoted_shadows_are_named_in_the_report() {
    let store = store();
    let eraser = eraser(&store);
    let team_ns = Namespace::Team("atlas".to_string());

    // A multi-parent derivative survives; a promoted global copy is named.
    let erased = episode("erased team source", team_ns.clone());
    let surviving = episode("surviving team source", team_ns.clone());
    let shared = fact("derived from both", team_ns.clone());
    let team_fact = fact("promoted team fact", team_ns.clone());
    let global_copy = fact("the global copy", Namespace::Global);
    store.insert_episode(&erased).expect("insert");
    store.insert_episode(&surviving).expect("insert");
    store.insert_fact(&shared).expect("insert");
    store.insert_fact(&team_fact).expect("insert");
    store.insert_fact(&global_copy).expect("insert");
    derived_fact_edge(&store, &shared.identity.id, &erased.identity.id);
    derived_fact_edge(&store, &shared.identity.id, &surviving.identity.id);
    derived_fact_edge(&store, &team_fact.identity.id, &erased.identity.id);
    promoted_edge(&store, &team_fact.identity.id, &global_copy.identity.id);

    let outcome = eraser
        .erase(&principal(), &PermitAll, &erased.identity.id, &now())
        .expect("erase");
    let PointErase::Erased(report) = outcome else {
        panic!("expected Erased, got {outcome:?}");
    };
    assert_eq!(
        report.spared_multiparent,
        vec![shared.identity.id],
        "the multi-parent derivative is named as spared"
    );
    assert_eq!(
        report.promoted_shadows,
        vec![global_copy.identity.id],
        "the cross-namespace copy is named, not followed"
    );
    assert!(is_live(&store, &shared.identity.id, "Fact"));
    assert!(
        is_live(&store, &global_copy.identity.id, "Fact"),
        "the global copy survives the core cascade"
    );
    assert!(!is_live(&store, &team_fact.identity.id, "Fact"));
}

#[test]
fn authorization_refuses_the_whole_cascade_before_any_write() {
    let store = store();
    let eraser = eraser(&store);
    let principal = principal();
    let own_ns = Namespace::Agent(principal.agent_id.to_string());

    // The seed is the principal's own; its derivative lives in a team the principal
    // does not belong to. Under the real default policy the seed's namespace passes
    // and the derivative's refuses — and one refusal covers the whole cascade.
    let e = episode("authorized seed", own_ns.clone());
    let f = fact(
        "derivative in someone else's team",
        Namespace::Team("atlas".to_string()),
    );
    store.insert_episode(&e).expect("insert");
    store.insert_fact(&f).expect("insert");
    derived_fact_edge(&store, &f.identity.id, &e.identity.id);

    let outcome = eraser
        .erase(&principal, &DefaultAuthorizer, &e.identity.id, &now())
        .expect("call");
    assert_eq!(
        outcome,
        PointErase::Unauthorized {
            namespace: Namespace::Team("atlas".to_string())
        },
        "the refusal names the namespace that denied"
    );
    assert!(
        is_live(&store, &e.identity.id, "Episode"),
        "nothing was touched"
    );
    assert!(is_live(&store, &f.identity.id, "Fact"));
    assert_eq!(
        store
            .audit_by_kind(AuditKind::Purge, None, 10)
            .expect("audit")
            .events
            .len(),
        0,
        "an unauthorized erase audits nothing"
    );

    // With membership in the spanned team, the same erase under the same default
    // policy applies — authorization is about the principal, not the cascade.
    let member = Principal::new(principal.agent_id, vec!["atlas".to_string()]);
    let outcome = eraser
        .erase(&member, &DefaultAuthorizer, &e.identity.id, &now())
        .expect("erase");
    assert!(
        matches!(outcome, PointErase::Erased(_)),
        "authorized end to end: {outcome:?}"
    );
    assert!(!is_live(&store, &e.identity.id, "Episode"));
    assert!(!is_live(&store, &f.identity.id, "Fact"));
}

#[test]
fn a_soft_forgotten_memory_erases() {
    let store = store();
    let eraser = eraser(&store);
    let f = fact("forgotten then erased", Namespace::Global);
    let f_node = store.insert_fact(&f).expect("insert");
    let forget_audit = AuditEvent {
        identity: identity_in(Namespace::Global),
        kind: AuditKind::Forget,
        subject_id: f.identity.id,
        actor_id: Id::from_content_hash(b"erase-agent"),
        payload: serde_json::json!({"reason": "manual"}),
        signature: String::new(),
        occurred_at: now(),
    };
    store
        .soft_forget(f_node, &now(), &forget_audit)
        .expect("soft forget");

    let outcome = eraser
        .erase(&principal(), &PermitAll, &f.identity.id, &now())
        .expect("erase");
    assert!(
        matches!(outcome, PointErase::Erased(_)),
        "erasure escalates past a soft-forget: {outcome:?}"
    );
    assert!(!is_live(&store, &f.identity.id, "Fact"));
}
