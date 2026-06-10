//! Engine acceptance for the forgetting facade (05 §2, M5.T02): the off-switch is
//! inert, the sweep forgets only the all-axes-low and spares every guarded class, a
//! resumed watermark walk equals one full pass, point ops report protections, and the
//! forget audit is visible to the owning agent through the scoped read facade.

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
use aionforge_engine::{
    ForgettingPolicy, Memory, MemoryConfig, PointForget, PointUnforget, SpareReason,
};
use aionforge_store::Store;
use common::{FakeEmbedder, migrated_store, ts};

fn now() -> Timestamp {
    "2026-06-06T12:00:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

/// Far enough before `now()` to clear the default 30-day minimum age.
fn long_ago() -> Timestamp {
    "2025-12-01T09:00:00-06:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn memory_with_forgetting(store: &Arc<Store>, enabled: bool) -> Memory<FakeEmbedder> {
    let config = MemoryConfig {
        forgetting: ForgettingPolicy {
            enabled,
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

fn low_fact(namespace: Namespace) -> Fact {
    Fact {
        identity: Identity {
            id: Id::generate(),
            ingested_at: long_ago(),
            namespace,
            expired_at: None,
        },
        stats: low_stats(),
        subject_id: Id::from_content_hash(b"subject"),
        predicate: "tests".to_string(),
        object: ObjectValue::Text("the facade".to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: "tests the facade".to_string(),
        embedding: None,
        embedder_model: None,
        extraction: None,
    }
}

fn low_episode(namespace: Namespace) -> Episode {
    let content = format!("episode {}", Id::generate());
    Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: long_ago(),
            namespace,
            expired_at: None,
        },
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
    }
}

fn is_expired(store: &Store, id: &Id) -> bool {
    store
        .memory_by_id(id, &["Episode", "Fact"])
        .expect("resolve")
        .map(|c| c.identity.expired_at.is_some())
        .unwrap_or(false)
}

#[test]
fn the_off_switch_is_inert_everywhere() {
    let store = migrated_store();
    let memory = memory_with_forgetting(&store, false);
    let fact = low_fact(Namespace::Global);
    store.insert_fact(&fact).expect("insert");

    let report = memory.sweep_forgetting(None, 200, &now()).expect("sweep");
    assert_eq!(report, aionforge_engine::ForgetSweepPage::default());
    assert_eq!(
        memory.forget(&fact.identity.id, &now()).expect("forget"),
        PointForget::Disabled
    );
    assert_eq!(
        memory
            .unforget(&fact.identity.id, &now())
            .expect("unforget"),
        PointUnforget::Disabled
    );
    assert!(!is_expired(&store, &fact.identity.id), "nothing changed");
    assert_eq!(
        store
            .audit_by_kind(AuditKind::Forget, None, 10)
            .expect("audit")
            .events
            .len(),
        0,
        "nothing audited"
    );
}

#[test]
fn the_sweep_forgets_eligible_spares_guarded_and_converges() {
    let store = migrated_store();
    let memory = memory_with_forgetting(&store, true);

    let owner = Principal::agent(Id::from_content_hash(b"tester"));
    let agent_ns = Namespace::Agent(owner.agent_id.to_string());
    let eligible = low_fact(agent_ns.clone());
    let eligible_episode = low_episode(agent_ns.clone());
    let pinned = Fact {
        stats: Stats {
            is_pinned: true,
            ..low_stats()
        },
        ..low_fact(Namespace::Global)
    };
    let trusted = Fact {
        stats: Stats {
            trust: 0.9,
            ..low_stats()
        },
        ..low_fact(Namespace::Global)
    };
    for f in [&eligible, &pinned, &trusted] {
        store.insert_fact(f).expect("insert");
    }
    store.insert_episode(&eligible_episode).expect("insert");

    let report = memory.sweep_forgetting(None, 200, &now()).expect("sweep");
    assert_eq!(report.scanned, 4);
    assert_eq!(report.forgotten, 2, "the eligible fact and episode");
    assert_eq!(report.spared, 2, "pinned and trusted");
    assert!(is_expired(&store, &eligible.identity.id));
    assert!(is_expired(&store, &eligible_episode.identity.id));
    assert!(!is_expired(&store, &pinned.identity.id));
    assert!(!is_expired(&store, &trusted.identity.id));

    // Idempotent: re-sweeping already-forgotten ground reads forgotten = 0.
    let again = memory
        .sweep_forgetting(None, 200, &now())
        .expect("re-sweep");
    assert_eq!(again.forgotten, 0);
    assert_eq!(again.scanned, 2, "only the spared remain unexpired");

    // The owning agent sees the forget through the scoped audit facade.
    let page = memory
        .audit_by_subject_kind(&owner, &eligible.identity.id, AuditKind::Forget, None, 10)
        .expect("scoped audit");
    assert_eq!(page.records.len(), 1, "the agent sees its own forget");
    assert_eq!(
        page.records[0].event.payload["reason"],
        "active_forgetting_sweep"
    );
}

#[test]
fn a_watermark_resume_equals_one_full_pass() {
    let store = migrated_store();
    let memory = memory_with_forgetting(&store, true);
    let facts: Vec<Fact> = (0..6).map(|_| low_fact(Namespace::Global)).collect();
    for f in &facts {
        store.insert_fact(f).expect("insert");
    }

    let mut cursor = None;
    let mut forgotten = 0;
    loop {
        // A small page each call, persisting the watermark between calls.
        let page = memory
            .sweep_forgetting(cursor.as_ref(), 2, &now())
            .expect("page");
        forgotten += page.forgotten;
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(forgotten, 6, "the resumed walk covers the whole population");
    for f in &facts {
        assert!(is_expired(&store, &f.identity.id));
    }
}

#[test]
fn point_ops_report_protections_and_round_trip() {
    let store = migrated_store();
    let memory = memory_with_forgetting(&store, true);

    let pinned = Fact {
        stats: Stats {
            is_pinned: true,
            ..low_stats()
        },
        ..low_fact(Namespace::Global)
    };
    store.insert_fact(&pinned).expect("insert");
    assert_eq!(
        memory.forget(&pinned.identity.id, &now()).expect("forget"),
        PointForget::Protected(SpareReason::Pinned)
    );

    let fact = low_fact(Namespace::Global);
    store.insert_fact(&fact).expect("insert");
    assert_eq!(
        memory.forget(&fact.identity.id, &now()).expect("forget"),
        PointForget::Forgotten
    );
    assert_eq!(
        memory
            .unforget(&fact.identity.id, &now())
            .expect("unforget"),
        PointUnforget::Restored
    );
    assert!(!is_expired(&store, &fact.identity.id));
}
