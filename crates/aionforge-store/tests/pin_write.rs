//! Store-level tests for the pin / unpin writes (05 §2, M5.T02 rider): one `is_pinned`
//! flip per op, audit co-committed and gated on a real transition, the deliberate
//! absence of any status or expiry gate, and the WAL round-trip.

use std::path::PathBuf;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::semantic::{Fact, FactStatus};
use aionforge_domain::time::Timestamp;
use aionforge_domain::value::ObjectValue;
use aionforge_store::{NodeId, PinWrite, Store, StoreConfig};

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
    let dir = std::env::temp_dir().join(format!("aionforge-pin-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn fact_with(status: FactStatus, expired: bool) -> Fact {
    Fact {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts("2026-06-01T09:00:00-05:00[America/Chicago]"),
            namespace: Namespace::Global,
            expired_at: expired.then(now),
        },
        stats: Stats {
            importance: 0.04,
            trust: 0.2,
            last_access: ts("2026-06-01T09:00:00-05:00[America/Chicago]"),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.1,
            is_pinned: false,
        },
        subject_id: Id::from_content_hash(b"subject"),
        predicate: "tests".to_string(),
        object: ObjectValue::Text("pin writes".to_string()),
        confidence: 0.9,
        status,
        statement: "tests pin writes".to_string(),
        embedding: None,
        embedder_model: None,
        extraction: None,
        cooled_until: None,
    }
}

/// A distinct, deterministic audit event per `(kind, seed)` — the cycle-id discipline is
/// the pinning surface's job; these tests only need distinct rows per real event.
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
        actor_id: Id::from_content_hash(b"test-pinner"),
        payload: serde_json::json!({"reason": "manual_pin"}),
        signature: String::new(),
        occurred_at: now(),
    }
}

/// Resolve a fact's node and current pin state through the public point-op resolver —
/// deliberately the expiry-blind reader, so expired fixtures resolve too.
fn node_and_pin(store: &Store, id: &Id) -> (NodeId, bool) {
    let candidate = store
        .memory_by_id(id, &["Fact"])
        .expect("resolve")
        .expect("present");
    (candidate.node, candidate.stats.is_pinned)
}

fn audit_count(store: &Store, kind: AuditKind) -> usize {
    store
        .audit_by_kind(kind, None, 200)
        .expect("audit page")
        .events
        .len()
}

#[test]
fn pin_is_gated_idempotent_and_audited_once() {
    let store = store();
    let fact = fact_with(FactStatus::Active, false);
    store.insert_fact(&fact).expect("insert");
    let (node, pinned) = node_and_pin(&store, &fact.identity.id);
    assert!(!pinned);

    let first = store
        .set_pinned(
            node,
            &audit_event(AuditKind::Pin, fact.identity.id, "pin-1"),
        )
        .expect("pin");
    assert_eq!(first, PinWrite::Applied);
    assert!(node_and_pin(&store, &fact.identity.id).1);
    assert_eq!(audit_count(&store, AuditKind::Pin), 1);

    // A replay — even with a different audit id — is a no-op with no second row: the
    // gate fires on state, before any audit is built into the graph.
    let replay = store
        .set_pinned(
            node,
            &audit_event(AuditKind::Pin, fact.identity.id, "pin-replay"),
        )
        .expect("replay");
    assert_eq!(replay, PinWrite::Noop);
    assert_eq!(audit_count(&store, AuditKind::Pin), 1, "single audit row");
}

#[test]
fn unpin_restores_and_never_fires_without_a_transition() {
    let store = store();
    let fact = fact_with(FactStatus::Active, false);
    store.insert_fact(&fact).expect("insert");
    let (node, _) = node_and_pin(&store, &fact.identity.id);

    // Unpin on a never-pinned node: no-op, no audit row.
    let nothing = store
        .clear_pinned(
            node,
            &audit_event(AuditKind::Unpin, fact.identity.id, "unpin-early"),
        )
        .expect("unpin");
    assert_eq!(nothing, PinWrite::Noop);
    assert_eq!(audit_count(&store, AuditKind::Unpin), 0);

    store
        .set_pinned(
            node,
            &audit_event(AuditKind::Pin, fact.identity.id, "pin-1"),
        )
        .expect("pin");
    let lifted = store
        .clear_pinned(
            node,
            &audit_event(AuditKind::Unpin, fact.identity.id, "unpin-1"),
        )
        .expect("unpin");
    assert_eq!(lifted, PinWrite::Applied);
    assert!(!node_and_pin(&store, &fact.identity.id).1);
    assert_eq!(audit_count(&store, AuditKind::Unpin), 1);

    // The full cycle leaves distinct decision rows, not one merged row.
    let again = store
        .set_pinned(
            node,
            &audit_event(AuditKind::Pin, fact.identity.id, "pin-2"),
        )
        .expect("re-pin");
    assert_eq!(again, PinWrite::Applied);
    assert_eq!(
        audit_count(&store, AuditKind::Pin),
        2,
        "pin -> unpin -> pin is three real events: two pins, one unpin"
    );
}

#[test]
fn neither_status_nor_expiry_gates_a_pin() {
    let store = store();
    // The contradiction-quarantine and supersession shapes the forget flip REFUSES —
    // the pin flip must accept both: is_pinned shares no lifecycle signature, and a
    // quarantined-but-recoverable memory is a legitimate thing to protect.
    let contradicted = fact_with(FactStatus::Quarantined, false);
    let superseded = fact_with(FactStatus::Superseded, false);
    // A soft-forgotten memory: pin protects it without restoring it.
    let forgotten = fact_with(FactStatus::Active, true);
    for f in [&contradicted, &superseded, &forgotten] {
        store.insert_fact(f).expect("insert");
    }

    for (name, fact) in [
        ("quarantined", &contradicted),
        ("superseded", &superseded),
        ("forgotten", &forgotten),
    ] {
        let (node, _) = node_and_pin(&store, &fact.identity.id);
        let outcome = store
            .set_pinned(node, &audit_event(AuditKind::Pin, fact.identity.id, name))
            .expect("pin");
        assert_eq!(outcome, PinWrite::Applied, "{name} accepts a pin");
        assert!(node_and_pin(&store, &fact.identity.id).1, "{name} pinned");
    }

    // The pin did not restore the forgotten memory: expired_at is untouched.
    let resolved = store
        .memory_by_id(&forgotten.identity.id, &["Fact"])
        .expect("resolve")
        .expect("present");
    assert!(
        resolved.identity.expired_at.is_some(),
        "pinning a forgotten memory protects it without un-forgetting it"
    );
}

#[test]
fn the_wal_round_trips_pin_state() {
    let dir = temp_dir("wal");
    let config = StoreConfig {
        embedding_dimension: 4,
    };
    let pinned = fact_with(FactStatus::Active, false);
    let cycled = fact_with(FactStatus::Active, false);
    {
        let store = Store::open_persistent_migrated(&dir, config, &now()).expect("open persistent");
        store.insert_fact(&pinned).expect("insert");
        store.insert_fact(&cycled).expect("insert");
        let (pinned_node, _) = node_and_pin(&store, &pinned.identity.id);
        let (cycled_node, _) = node_and_pin(&store, &cycled.identity.id);
        store
            .set_pinned(
                pinned_node,
                &audit_event(AuditKind::Pin, pinned.identity.id, "wal-p1"),
            )
            .expect("pin kept");
        store
            .set_pinned(
                cycled_node,
                &audit_event(AuditKind::Pin, cycled.identity.id, "wal-p2"),
            )
            .expect("pin cycled");
        store
            .clear_pinned(
                cycled_node,
                &audit_event(AuditKind::Unpin, cycled.identity.id, "wal-u1"),
            )
            .expect("unpin cycled");
        drop(store);
    }

    let recovered = Store::recover(&dir, config, &now()).expect("recover");
    assert!(
        node_and_pin(&recovered, &pinned.identity.id).1,
        "a pinned memory stays pinned across recovery"
    );
    assert!(
        !node_and_pin(&recovered, &cycled.identity.id).1,
        "pin-then-unpin recovers to unpinned"
    );
    assert_eq!(audit_count(&recovered, AuditKind::Pin), 2);
    assert_eq!(audit_count(&recovered, AuditKind::Unpin), 1);
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
fn an_installed_signer_stamps_pin_audits_at_commit() {
    let store = store();
    store
        .install_audit_signer(std::sync::Arc::new(FakeSigner))
        .expect("install signer");
    let fact = fact_with(FactStatus::Active, false);
    store.insert_fact(&fact).expect("insert");
    let (node, _) = node_and_pin(&store, &fact.identity.id);

    let event = audit_event(AuditKind::Pin, fact.identity.id, "signed-pin");
    assert_eq!(
        store.set_pinned(node, &event).expect("pin"),
        PinWrite::Applied
    );
    let row = &store
        .audit_by_kind(AuditKind::Pin, None, 10)
        .expect("audit")
        .events[0];
    assert_eq!(
        row.signature,
        format!("fake-sig|{}", event.identity.id),
        "the blank pin event was stamped inside the commit by the installed signer"
    );

    // The replay path under a signer is still a state-gated no-op: no second row,
    // and the stored signature is untouched.
    assert_eq!(
        store
            .set_pinned(
                node,
                &audit_event(AuditKind::Pin, fact.identity.id, "signed-pin-replay"),
            )
            .expect("replay"),
        PinWrite::Noop
    );
    assert_eq!(audit_count(&store, AuditKind::Pin), 1);
}
