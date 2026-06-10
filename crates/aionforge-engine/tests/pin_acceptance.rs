//! Facade acceptance for pin/unpin (05 §2, M5.T02 rider). The load-bearing clause: the
//! ops work under the **shipped default configuration** — forgetting off, no forgetter
//! built — because the pin's first consumer is read-time decay and a pin can only ever
//! spare. The forget surfaces answer `Disabled` on the same engine at the same time,
//! which is exactly the contrast the design intends.

mod common;

use std::sync::Arc;

use aionforge_domain::authz::Principal;
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::AuditKind;
use aionforge_domain::nodes::semantic::{Fact, FactStatus};
use aionforge_domain::time::Timestamp;
use aionforge_domain::value::ObjectValue;
use aionforge_engine::{ForgettingPolicy, Memory, MemoryConfig, PointForget, PointPin, PointUnpin};
use aionforge_store::Store;
use common::{FakeEmbedder, migrated_store, ts};

fn now() -> Timestamp {
    "2026-06-06T12:00:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn long_ago() -> Timestamp {
    "2025-12-01T09:00:00-06:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn memory(store: &Arc<Store>, forgetting: bool) -> Memory<FakeEmbedder> {
    let config = MemoryConfig {
        forgetting: ForgettingPolicy {
            enabled: forgetting,
            ..ForgettingPolicy::default()
        },
        ..MemoryConfig::default()
    };
    Memory::new(Arc::clone(store), FakeEmbedder::new(), config, &ts(0)).expect("memory")
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

fn low_episode(content: &str, namespace: Namespace) -> Episode {
    Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: long_ago(),
            namespace,
            expired_at: None,
        },
        stats: low_stats(),
        content: content.to_string(),
        role: Role::User,
        captured_at: long_ago(),
        agent_id: Id::from_content_hash(b"pin-agent"),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    }
}

fn low_fact() -> Fact {
    Fact {
        identity: Identity {
            id: Id::generate(),
            ingested_at: long_ago(),
            namespace: Namespace::Global,
            expired_at: None,
        },
        stats: low_stats(),
        subject_id: Id::from_content_hash(b"subject"),
        predicate: "tests".to_string(),
        object: ObjectValue::Text("pin acceptance".to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: "tests pin acceptance".to_string(),
        embedding: None,
        embedder_model: None,
        extraction: None,
    }
}

#[tokio::test]
async fn pin_works_under_the_default_configuration_where_forget_is_disabled() {
    let store = migrated_store();
    // Forgetting OFF — the shipped default. The engine builds no forgetter.
    let memory = memory(&store, false);
    let owner = Principal::agent(Id::from_content_hash(b"pin-agent"));
    let ns = Namespace::Agent(owner.agent_id.to_string());
    let episode = low_episode("a hard-won lesson", ns.clone());
    store.insert_episode(&episode).expect("insert");

    // The contrast pair: forget answers Disabled, pin works.
    assert_eq!(
        memory.forget(&episode.identity.id, &now()).expect("forget"),
        PointForget::Disabled,
        "forgetting is off on this engine"
    );
    assert_eq!(
        memory.pin(&episode.identity.id, &now()).expect("pin"),
        PointPin::Pinned,
        "the pin is always available"
    );
    assert_eq!(
        memory.pin(&episode.identity.id, &now()).expect("replay"),
        PointPin::AlreadyPinned
    );

    // The agent sees its own pin history through the scoped audit read.
    let pins = memory
        .audit_by_subject_kind(&owner, &episode.identity.id, AuditKind::Pin, None, 10)
        .expect("audit");
    assert_eq!(pins.records.len(), 1, "one pin decision, one row");
    assert_eq!(pins.records[0].event.identity.namespace, ns);
    assert_eq!(pins.records[0].event.payload["reason"], "manual_pin");

    assert_eq!(
        memory.unpin(&episode.identity.id, &now()).expect("unpin"),
        PointUnpin::Unpinned
    );
    let unpins = memory
        .audit_by_subject_kind(&owner, &episode.identity.id, AuditKind::Unpin, None, 10)
        .expect("audit");
    assert_eq!(unpins.records.len(), 1);

    // Unknown id stays an honest NotFound.
    assert_eq!(
        memory.pin(&Id::generate(), &now()).expect("pin"),
        PointPin::NotFound
    );
}

#[tokio::test]
async fn a_facade_pin_holds_against_the_sweep_until_lifted() {
    let store = migrated_store();
    let memory = memory(&store, true);
    let fact = low_fact();
    store.insert_fact(&fact).expect("insert");

    assert_eq!(
        memory.pin(&fact.identity.id, &now()).expect("pin"),
        PointPin::Pinned
    );
    let swept = memory.sweep_forgetting(None, 200, &now()).expect("sweep");
    assert_eq!(swept.forgotten, 0, "the pinned all-axes-low fact survives");
    assert_eq!(swept.spared, 1);

    assert_eq!(
        memory.unpin(&fact.identity.id, &now()).expect("unpin"),
        PointUnpin::Unpinned
    );
    let reswept = memory
        .sweep_forgetting(None, 200, &now())
        .expect("re-sweep");
    assert_eq!(
        reswept.forgotten, 1,
        "lifting the pin re-armed the sweep — a stay, not a vault"
    );
}
