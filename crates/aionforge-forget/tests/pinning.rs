//! Point-op acceptance for pin/unpin (05 §2, M5.T02 rider): the ops resolve every
//! `Stats`-bearing kind, audit in the memory's own namespace with the cycle discipline,
//! protect a soft-forgotten memory without restoring it, and interact with the sweep as
//! a stay, not a vault.

use std::sync::Arc;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::AuditKind;
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::time::Timestamp;
use aionforge_domain::value::ObjectValue;
use aionforge_forget::{
    Forgetter, ForgettingPolicy, PointForget, PointPin, PointUnforget, PointUnpin, pin, unpin,
};
use aionforge_store::{Store, StoreConfig};

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

fn now() -> Timestamp {
    ts("2026-06-06T12:00:00-05:00[America/Chicago]")
}

/// Five months before `now`: past any sane min-age.
fn long_ago() -> Timestamp {
    ts("2026-01-05T09:00:00-06:00[America/Chicago]")
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

fn low_stats() -> Stats {
    Stats {
        importance: 0.04,
        trust: 0.2,
        last_access: long_ago(),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

fn identity_in(namespace: Namespace) -> Identity {
    Identity {
        id: Id::generate(),
        ingested_at: long_ago(),
        namespace,
        expired_at: None,
    }
}

fn low_fact() -> Fact {
    Fact {
        identity: identity_in(Namespace::Global),
        stats: low_stats(),
        subject_id: Id::from_content_hash(b"subject"),
        predicate: "tests".to_string(),
        object: ObjectValue::Text("pinning".to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: "tests pinning".to_string(),
        embedding: None,
        embedder_model: None,
        extraction: None,
    }
}

fn is_pinned(store: &Store, id: &Id, label: &str) -> bool {
    store
        .memory_by_id(id, &[label])
        .expect("resolve")
        .expect("present")
        .stats
        .is_pinned
}

#[test]
fn pin_round_trips_resolves_every_kind_and_audits_in_the_own_namespace() {
    let store = store();

    // Unknown id.
    assert_eq!(
        pin(&store, &Id::generate(), &now()).expect("pin"),
        PointPin::NotFound
    );
    assert_eq!(
        unpin(&store, &Id::generate(), &now()).expect("unpin"),
        PointUnpin::NotFound
    );

    // An entity — outside the point-FORGET label set — pins fine: the pin surface
    // resolves every Stats-bearing kind, because a pin can only spare.
    let entity = Entity {
        identity: identity_in(Namespace::Global),
        stats: low_stats(),
        canonical_name: "selene".to_string(),
        entity_type: "Project".to_string(),
        aliases: Vec::new(),
        description: None,
        embedding: None,
        embedder_model: None,
        attributes: None,
    };
    store.insert_entity(&entity).expect("insert entity");
    assert_eq!(
        pin(&store, &entity.identity.id, &now()).expect("pin"),
        PointPin::Pinned
    );
    assert!(is_pinned(&store, &entity.identity.id, "Entity"));

    // An episode in an agent namespace: the audit row lands in the memory's OWN
    // namespace, with the terse reason-and-kind payload.
    let content = "pin acceptance".to_string();
    let episode = Episode {
        identity: identity_in(Namespace::Agent("tester".to_string())),
        stats: low_stats(),
        content: content.clone(),
        role: Role::User,
        captured_at: long_ago(),
        agent_id: Id::from_content_hash(b"test-agent"),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("insert episode");
    assert_eq!(
        pin(&store, &episode.identity.id, &now()).expect("pin"),
        PointPin::Pinned
    );
    assert_eq!(
        pin(&store, &episode.identity.id, &now()).expect("replay"),
        PointPin::AlreadyPinned
    );
    let rows = store
        .audit_by_kind(AuditKind::Pin, None, 10)
        .expect("audit")
        .events;
    assert_eq!(
        rows.len(),
        2,
        "one row per applied pin, none for the replay"
    );
    let episode_row = rows
        .iter()
        .find(|e| e.subject_id == episode.identity.id)
        .expect("episode pin audited");
    assert_eq!(
        episode_row.identity.namespace,
        Namespace::Agent("tester".to_string()),
        "audited in the memory's own namespace"
    );
    assert_eq!(episode_row.payload["reason"], "manual_pin");
    assert_eq!(episode_row.payload["kind"], "Episode");

    // The way back, with its own audit kind.
    assert_eq!(
        unpin(&store, &episode.identity.id, &now()).expect("unpin"),
        PointUnpin::Unpinned
    );
    assert_eq!(
        unpin(&store, &episode.identity.id, &now()).expect("replay"),
        PointUnpin::NotPinned
    );
    let unpin_rows = store
        .audit_by_kind(AuditKind::Unpin, None, 10)
        .expect("audit")
        .events;
    assert_eq!(unpin_rows.len(), 1);
    assert_eq!(unpin_rows[0].payload["reason"], "manual_unpin");
}

#[test]
fn a_pin_is_a_stay_not_a_vault() {
    let store = store();
    let forgetter = Forgetter::new(
        Arc::clone(&store),
        ForgettingPolicy {
            enabled: true,
            ..ForgettingPolicy::default()
        },
    );
    let fact = low_fact();
    store.insert_fact(&fact).expect("insert");

    // Pinned: the all-axes-low fact survives the sweep.
    assert_eq!(
        pin(&store, &fact.identity.id, &now()).expect("pin"),
        PointPin::Pinned
    );
    let swept = forgetter.sweep_page(None, 200, &now()).expect("sweep");
    assert_eq!(swept.forgotten, 0, "the pin spares");
    assert_eq!(swept.spared, 1);

    // Unpinned: eligibility re-arms silently, and the next sweep forgets it because
    // every other axis still holds low — the pin was a stay, not a vault.
    assert_eq!(
        unpin(&store, &fact.identity.id, &now()).expect("unpin"),
        PointUnpin::Unpinned
    );
    let reswept = forgetter.sweep_page(None, 200, &now()).expect("re-sweep");
    assert_eq!(reswept.forgotten, 1, "unpin re-armed the sweep");
}

#[test]
fn pinning_a_forgotten_memory_protects_without_restoring() {
    let store = store();
    let forgetter = Forgetter::new(
        Arc::clone(&store),
        ForgettingPolicy {
            enabled: true,
            ..ForgettingPolicy::default()
        },
    );
    let fact = low_fact();
    store.insert_fact(&fact).expect("insert");

    assert_eq!(
        forgetter.forget(&fact.identity.id, &now()).expect("forget"),
        PointForget::Forgotten
    );
    // Pin the forgotten memory: protected, but still out of default recall.
    assert_eq!(
        pin(&store, &fact.identity.id, &now()).expect("pin"),
        PointPin::Pinned
    );
    let resolved = store
        .memory_by_id(&fact.identity.id, &["Fact"])
        .expect("resolve")
        .expect("present");
    assert!(
        resolved.identity.expired_at.is_some(),
        "the pin never clears expired_at — un-forgetting is its own transition"
    );
    assert!(resolved.stats.is_pinned);

    // Restore it: still pinned afterwards, and now sweep-proof.
    assert_eq!(
        forgetter
            .unforget(&fact.identity.id, &now())
            .expect("unforget"),
        PointUnforget::Restored
    );
    assert!(is_pinned(&store, &fact.identity.id, "Fact"));
    let swept = forgetter.sweep_page(None, 200, &now()).expect("sweep");
    assert_eq!(
        swept.forgotten, 0,
        "restored and pinned: the sweep spares it"
    );
}
