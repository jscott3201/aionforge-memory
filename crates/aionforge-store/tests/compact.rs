//! Store-level tests for physical reclaim (05 §3, M5.T03): a purge leaves reclaimable
//! rows, compaction drops them to zero and loses nothing live, and a compacted store
//! still recovers from its WAL.

use std::path::PathBuf;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::embedding::Embedding;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::Timestamp;
use aionforge_store::{SearchKind, Store, StoreConfig};

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
    let dir =
        std::env::temp_dir().join(format!("aionforge-compact-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn episode(content: &str, axis: usize) -> Episode {
    let mut vector = vec![0.0; 4];
    vector[axis] = 1.0;
    Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts("2026-06-01T09:00:00-05:00[America/Chicago]"),
            namespace: Namespace::Global,
            expired_at: None,
        },
        stats: Stats {
            importance: 0.5,
            trust: 0.5,
            last_access: ts("2026-06-01T09:00:00-05:00[America/Chicago]"),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.1,
            is_pinned: false,
        },
        content: content.to_string(),
        role: Role::User,
        captured_at: now(),
        agent_id: Id::from_content_hash(b"compact-agent"),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: Some(Embedding::new(vector).expect("embedding")),
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    }
}

fn purge_audit(seed: Id, tag: &str) -> AuditEvent {
    AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(tag.as_bytes()),
            ingested_at: now(),
            namespace: Namespace::Global,
            expired_at: None,
        },
        kind: AuditKind::Purge,
        subject_id: seed,
        actor_id: Id::from_content_hash(b"compact-agent"),
        payload: serde_json::json!({"reason": "right_to_erasure"}),
        signature: String::new(),
        occurred_at: now(),
    }
}

#[test]
fn compaction_reclaims_purged_rows_and_loses_nothing_live() {
    let store = store();
    let erased = episode("erased and reclaimed", 0);
    let keeper = episode("alive throughout", 1);
    let erased_node = store.insert_episode(&erased).expect("insert");
    let keeper_node = store.insert_episode(&keeper).expect("insert");

    store
        .hard_purge(
            &[erased_node],
            &purge_audit(erased.identity.id, "compact-purge"),
        )
        .expect("purge");
    let before = store.compaction_pressure();
    assert!(
        before.reclaimable_nodes >= 1,
        "the purge left a reclaimable dead row: {before:?}"
    );

    let report = store.compact().expect("compact");
    assert!(
        report.reclaimed_nodes >= 1,
        "the dead row was dropped: {report:?}"
    );
    let after = store.compaction_pressure();
    assert_eq!(after.reclaimable_nodes, 0, "nothing left to reclaim");
    assert_eq!(after.reclaimable_edges, 0);

    // Nothing live was lost: the keeper resolves, searches, and the purged one stays
    // gone through the rebuilt indexes.
    assert!(
        store
            .memory_by_id(&keeper.identity.id, &["Episode"])
            .expect("resolve")
            .is_some(),
        "the surviving episode resolves after the rebuild"
    );
    let hits = store
        .text_search(SearchKind::Episode, "alive", 5)
        .expect("search");
    assert!(hits.iter().any(|hit| hit.node == keeper_node));
    assert!(
        store
            .text_search(SearchKind::Episode, "reclaimed", 5)
            .expect("search")
            .is_empty(),
        "the purged content stays unreachable through the rebuilt text index"
    );
    let query = Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("embedding");
    assert!(
        !store
            .vector_search_exact(SearchKind::Episode, &query, 5)
            .expect("search")
            .iter()
            .any(|hit| hit.node == erased_node),
        "the rebuilt vector index carries no entry for the purged node"
    );

    // An idle pass is a lossless no-op.
    let idle = store.compact().expect("idle compact");
    assert_eq!(
        idle,
        aionforge_store::CompactReport {
            reclaimed_nodes: 0,
            reclaimed_edges: 0,
        }
    );
}

#[test]
fn a_compacted_store_still_recovers_from_its_wal() {
    let dir = temp_dir("wal");
    let config = StoreConfig {
        embedding_dimension: 4,
    };
    let erased = episode("purged then compacted", 0);
    let keeper = episode("survives recovery", 1);
    {
        let store = Store::open_persistent_migrated(&dir, config, &now()).expect("open persistent");
        let erased_node = store.insert_episode(&erased).expect("insert");
        store.insert_episode(&keeper).expect("insert");
        store
            .hard_purge(
                &[erased_node],
                &purge_audit(erased.identity.id, "compact-wal"),
            )
            .expect("purge");
        store.compact().expect("compact");
        drop(store);
    }

    // Compaction writes no snapshot: recovery replays the full WAL from the empty
    // baseline and converges on the same state — purged stays purged, keeper lives.
    let recovered = Store::recover(&dir, config).expect("recover");
    assert!(
        recovered
            .memory_by_id(&erased.identity.id, &["Episode"])
            .expect("resolve")
            .is_none(),
        "the purged episode stays purged across recovery"
    );
    assert!(
        recovered
            .memory_by_id(&keeper.identity.id, &["Episode"])
            .expect("resolve")
            .is_some(),
        "the surviving episode recovers"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edge_counters_move_through_the_full_reclaim_arc() {
    let store = store();
    let erased = episode("edge-carrying erased source", 0);
    let derivative = episode("derived and erased with it", 1);
    let e_node = store.insert_episode(&erased).expect("insert");
    let d_node = store.insert_episode(&derivative).expect("insert");
    let bound = aionforge_store::BoundQuery::new(
        "MATCH (a:Episode {id: $from}), (b:Episode {id: $to}) \
         INSERT (a)-[:DERIVED_FROM {derived_at: $ts}]->(b)",
    )
    .bind_uuid("from", derivative.identity.id)
    .unwrap()
    .bind_uuid("to", erased.identity.id)
    .unwrap()
    .bind("ts", aionforge_store::Value::ZonedDateTime(Box::new(now())))
    .unwrap();
    store.execute(&bound).expect("insert DERIVED_FROM edge");

    store
        .hard_purge(
            &[e_node, d_node],
            &purge_audit(erased.identity.id, "compact-edges"),
        )
        .expect("purge");
    let before = store.compaction_pressure();
    assert!(
        before.reclaimable_edges >= 1,
        "the severed edge is a reclaimable row: {before:?}"
    );
    assert!(before.allocated_nodes > before.live_nodes);

    let report = store.compact().expect("compact");
    assert!(
        report.reclaimed_edges >= 1,
        "the dead edge row was dropped: {report:?}"
    );
    let after = store.compaction_pressure();
    assert_eq!(after.reclaimable_edges, 0);
    assert_eq!(
        after.allocated_nodes, after.live_nodes,
        "dense after the rebuild: every allocated row is live"
    );
}
