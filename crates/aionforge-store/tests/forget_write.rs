//! Store-level tests for the soft-forget / unforget writes (05 §2, M5.T02): one
//! `expired_at` flip per op, audit co-committed and gated on a real transition, status
//! and every edge untouched, the non-`Active`-status refusal on both directions, and the
//! WAL round-trip.

use std::path::PathBuf;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::semantic::{Fact, FactStatus};
use aionforge_domain::time::Timestamp;
use aionforge_domain::value::ObjectValue;
use aionforge_store::{BoundQuery, ForgetWrite, NodeId, Store, StoreConfig, Value};

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
    let dir = std::env::temp_dir().join(format!("aionforge-forget-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn identity(expired: bool) -> Identity {
    Identity {
        id: Id::generate(),
        ingested_at: ts("2026-06-01T09:00:00-05:00[America/Chicago]"),
        namespace: Namespace::Global,
        expired_at: expired.then(now),
    }
}

fn stats() -> Stats {
    Stats {
        importance: 0.04,
        trust: 0.2,
        last_access: ts("2026-06-01T09:00:00-05:00[America/Chicago]"),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

fn fact_with(status: FactStatus, expired: bool) -> Fact {
    Fact {
        identity: identity(expired),
        stats: stats(),
        subject_id: Id::from_content_hash(b"subject"),
        predicate: "tests".to_string(),
        object: ObjectValue::Text("forget writes".to_string()),
        confidence: 0.9,
        status,
        statement: "tests forget writes".to_string(),
        embedding: None,
        embedder_model: None,
        extraction: None,
        cooled_until: None,
    }
}

fn episode() -> Episode {
    let content = format!("episode {}", Id::generate());
    Episode {
        identity: identity(false),
        stats: stats(),
        content: content.clone(),
        role: Role::User,
        captured_at: now(),
        agent_id: Id::from_content_hash(b"test-agent"),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    }
}

/// A distinct, deterministic audit event per `(kind, seed)` — the cycle-id discipline is
/// the orchestrator's job (PR-5); these tests only need distinct rows per real event.
fn audit_event(kind: AuditKind, subject: Id, seed: &str) -> AuditEvent {
    AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(seed.as_bytes()),
            ingested_at: now(),
            namespace: Namespace::Global,
            expired_at: None,
        },
        kind,
        subject_id: subject,
        actor_id: Id::from_content_hash(b"test-sweeper"),
        payload: serde_json::json!({"reason": "active_forgetting_sweep"}),
        signature: String::new(),
        occurred_at: now(),
    }
}

fn candidate_node(store: &Store, id: &Id) -> Option<NodeId> {
    store
        .forgettable_candidates(None, 200)
        .expect("page")
        .candidates
        .iter()
        .find(|c| c.identity.id == *id)
        .map(|c| c.node)
}

fn audit_count(store: &Store, kind: AuditKind) -> usize {
    store
        .audit_by_kind(kind, None, 200)
        .expect("audit page")
        .events
        .len()
}

fn support_edge(store: &Store, from: &Id, to: &Id) {
    let query = BoundQuery::new(
        "MATCH (a:Fact {id: $from}), (b:Fact {id: $to}) \
         INSERT (a)-[:SUPPORTS {weight: $weight}]->(b)",
    )
    .bind_uuid("from", from)
    .unwrap()
    .bind_uuid("to", to)
    .unwrap()
    .bind("weight", Value::Float(1.0))
    .unwrap();
    store.execute(&query).expect("insert SUPPORTS edge");
}

#[test]
fn forget_is_gated_idempotent_and_audited_once() {
    let store = store();
    let fact = fact_with(FactStatus::Active, false);
    store.insert_fact(&fact).expect("insert");
    let node = candidate_node(&store, &fact.identity.id).expect("candidate");

    let first = store
        .soft_forget(
            node,
            &now(),
            &audit_event(AuditKind::Forget, fact.identity.id, "forget-1"),
        )
        .expect("forget");
    assert_eq!(first, ForgetWrite::Applied);
    assert!(
        candidate_node(&store, &fact.identity.id).is_none(),
        "a forgotten node leaves the candidate page"
    );
    assert_eq!(audit_count(&store, AuditKind::Forget), 1);
    let row = &store
        .audit_by_kind(AuditKind::Forget, None, 10)
        .expect("page")
        .events[0];
    assert_eq!(row.subject_id, fact.identity.id);
    assert_eq!(
        row.identity.namespace,
        Namespace::Global,
        "the funnel preserves the caller-minted namespace (the orchestrator addresses \
         events to the memory's own namespace; the store never rewrites it)"
    );

    // A replay — even with a different audit id — is a no-op with no second row: the
    // gate fires on state, before any audit is built into the graph.
    let replay = store
        .soft_forget(
            node,
            &now(),
            &audit_event(AuditKind::Forget, fact.identity.id, "forget-replay"),
        )
        .expect("replay");
    assert_eq!(replay, ForgetWrite::Noop);
    assert_eq!(
        audit_count(&store, AuditKind::Forget),
        1,
        "single audit row"
    );
}

#[test]
fn unforget_restores_and_never_fires_without_a_transition() {
    let store = store();
    let fact = fact_with(FactStatus::Active, false);
    store.insert_fact(&fact).expect("insert");
    let node = candidate_node(&store, &fact.identity.id).expect("candidate");

    // Unforget on a never-forgotten node: no-op, no audit row.
    let nothing = store
        .unforget(
            node,
            &audit_event(AuditKind::Unforget, fact.identity.id, "unforget-early"),
        )
        .expect("unforget");
    assert_eq!(nothing, ForgetWrite::Noop);
    assert_eq!(audit_count(&store, AuditKind::Unforget), 0);

    store
        .soft_forget(
            node,
            &now(),
            &audit_event(AuditKind::Forget, fact.identity.id, "forget-1"),
        )
        .expect("forget");
    let restored = store
        .unforget(
            node,
            &audit_event(AuditKind::Unforget, fact.identity.id, "unforget-1"),
        )
        .expect("unforget");
    assert_eq!(restored, ForgetWrite::Applied);
    assert!(
        candidate_node(&store, &fact.identity.id).is_some(),
        "an unforgotten node re-enters the candidate page"
    );
    assert_eq!(audit_count(&store, AuditKind::Unforget), 1);

    // The full cycle leaves two distinct decision rows, not one merged row.
    let again = store
        .soft_forget(
            node,
            &now(),
            &audit_event(AuditKind::Forget, fact.identity.id, "forget-2"),
        )
        .expect("re-forget");
    assert_eq!(again, ForgetWrite::Applied);
    assert_eq!(
        audit_count(&store, AuditKind::Forget),
        2,
        "forget -> unforget -> forget is three real events: two forgets, one unforget"
    );
}

#[test]
fn non_active_status_is_refused_in_both_directions() {
    let store = store();
    // The demotion signature: expired_at paired with Quarantined. Unforget must refuse —
    // that expiry belongs to governance, and clearing it would resurrect a retired fact.
    let demoted_shape = fact_with(FactStatus::Quarantined, true);
    // Contradiction quarantine: status flipped, expired_at still None. Soft-forget must
    // refuse — writing expired_at here would manufacture the demotion signature.
    let contradicted = fact_with(FactStatus::Quarantined, false);
    let superseded = fact_with(FactStatus::Superseded, false);
    for f in [&demoted_shape, &contradicted, &superseded] {
        store.insert_fact(f).expect("insert");
    }
    // The two unexpired fixtures are visible on the candidate page (it filters expiry,
    // not status); the demoted shape is not, so resolve it via the subject index.
    let contradicted_node = candidate_node(&store, &contradicted.identity.id).expect("on page");
    let superseded_node = candidate_node(&store, &superseded.identity.id).expect("on page");
    let all = store
        .facts_by_subject(&Id::from_content_hash(b"subject"))
        .expect("subject lookup");
    let demoted_node = *all
        .iter()
        .find(|n| ![contradicted_node, superseded_node].contains(n))
        .expect("demoted node");

    assert_eq!(
        store
            .soft_forget(
                contradicted_node,
                &now(),
                &audit_event(AuditKind::Forget, contradicted.identity.id, "f-contra"),
            )
            .expect("call"),
        ForgetWrite::RefusedStatus
    );
    assert_eq!(
        store
            .soft_forget(
                superseded_node,
                &now(),
                &audit_event(AuditKind::Forget, superseded.identity.id, "f-super"),
            )
            .expect("call"),
        ForgetWrite::RefusedStatus
    );
    assert_eq!(
        store
            .unforget(
                demoted_node,
                &audit_event(AuditKind::Unforget, demoted_shape.identity.id, "u-demoted"),
            )
            .expect("call"),
        ForgetWrite::RefusedStatus
    );
    // No refusal emitted an audit row.
    assert_eq!(audit_count(&store, AuditKind::Forget), 0);
    assert_eq!(audit_count(&store, AuditKind::Unforget), 0);
}

#[test]
fn edges_survive_a_forget_untouched() {
    let store = store();
    let supported = fact_with(FactStatus::Active, false);
    let supporter = fact_with(FactStatus::Active, false);
    store.insert_fact(&supported).expect("insert");
    store.insert_fact(&supporter).expect("insert");
    support_edge(&store, &supporter.identity.id, &supported.identity.id);
    let node = candidate_node(&store, &supported.identity.id).expect("candidate");

    store
        .soft_forget(
            node,
            &now(),
            &audit_event(AuditKind::Forget, supported.identity.id, "f-edges"),
        )
        .expect("forget");
    assert!(
        store
            .has_protecting_reference(node, &["SUPPORTS"])
            .expect("probe"),
        "soft-forget writes no edge: the incoming SUPPORTS is still live"
    );
}

#[test]
fn an_episode_forgets_and_restores_without_a_status_block() {
    let store = store();
    let episode = episode();
    store.insert_episode(&episode).expect("insert");
    let node = candidate_node(&store, &episode.identity.id).expect("candidate");

    assert_eq!(
        store
            .soft_forget(
                node,
                &now(),
                &audit_event(AuditKind::Forget, episode.identity.id, "f-episode"),
            )
            .expect("forget"),
        ForgetWrite::Applied
    );
    assert!(candidate_node(&store, &episode.identity.id).is_none());
    assert_eq!(
        store
            .unforget(
                node,
                &audit_event(AuditKind::Unforget, episode.identity.id, "u-episode"),
            )
            .expect("unforget"),
        ForgetWrite::Applied
    );
    assert!(candidate_node(&store, &episode.identity.id).is_some());
}

#[test]
fn the_wal_round_trips_forget_state() {
    let dir = temp_dir("wal");
    let config = StoreConfig {
        embedding_dimension: 4,
    };
    let kept = fact_with(FactStatus::Active, false);
    let restored = fact_with(FactStatus::Active, false);
    {
        let store = Store::open_persistent_migrated(&dir, config, &now()).expect("open persistent");
        store.insert_fact(&kept).expect("insert");
        store.insert_fact(&restored).expect("insert");
        let kept_node = candidate_node(&store, &kept.identity.id).expect("candidate");
        let restored_node = candidate_node(&store, &restored.identity.id).expect("candidate");
        store
            .soft_forget(
                kept_node,
                &now(),
                &audit_event(AuditKind::Forget, kept.identity.id, "wal-f1"),
            )
            .expect("forget kept");
        store
            .soft_forget(
                restored_node,
                &now(),
                &audit_event(AuditKind::Forget, restored.identity.id, "wal-f2"),
            )
            .expect("forget restored");
        store
            .unforget(
                restored_node,
                &audit_event(AuditKind::Unforget, restored.identity.id, "wal-u1"),
            )
            .expect("unforget restored");
        drop(store);
    }

    let recovered = Store::recover(&dir, config).expect("recover");
    assert!(
        candidate_node(&recovered, &kept.identity.id).is_none(),
        "a forgotten memory stays forgotten across recovery"
    );
    assert!(
        candidate_node(&recovered, &restored.identity.id).is_some(),
        "forget-then-unforget recovers to expired_at absent"
    );
    assert_eq!(audit_count(&recovered, AuditKind::Forget), 2);
    assert_eq!(audit_count(&recovered, AuditKind::Unforget), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
