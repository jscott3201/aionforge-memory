//! Acceptance test for additive schema reconciliation on recovery.
//!
//! Pins the schema-additive slice of the greenfield-tax fix: a store first migrated
//! under an older catalog gains any catalog node/edge TYPE it is missing on the next
//! `open_or_recover` — additively (no fresh store), non-lossily (existing rows survive),
//! with its catalog indexes rebuilt and the recorded `SchemaVersion` advanced once the
//! binding declares the full catalog. Companion to the index reconciliations exercised in
//! `reconcile.rs` / `persistence.rs`.

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_store::{BoundQuery, QueryResult, SCHEMA_VERSION, Store, StoreConfig, Value};

use jiff::Zoned;

/// A stable [`Id`] for a string key, so synthetic UUID columns join consistently.
fn tag_id(key: &str) -> Id {
    Id::from_content_hash(key.as_bytes())
}

fn now() -> Zoned {
    "2026-06-06T12:00:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn zdt() -> Value {
    Value::ZonedDateTime(Box::new(now()))
}

/// A fresh, empty temp directory unique to `label`, removed first so re-runs start clean.
fn temp_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aionforge-recover-additive-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn insert_raw_episode(store: &Store, content: &str, captured_at: &str) -> Id {
    let captured_at: Zoned = captured_at
        .parse::<jiff::Timestamp>()
        .expect("valid captured_at")
        .to_zoned(jiff::tz::TimeZone::UTC);
    let id = tag_id(content);
    let episode = Episode {
        identity: Identity {
            id,
            ingested_at: now(),
            namespace: Namespace::Agent("test".to_string()),
            expired_at: None,
        },
        stats: Stats {
            importance: 0.5,
            trust: 0.8,
            last_access: now(),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.0,
            is_pinned: false,
        },
        content: content.to_string(),
        role: Role::User,
        captured_at,
        agent_id: tag_id("agent:test"),
        session_id: Some(tag_id("session:test")),
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("insert episode");
    id
}

fn episode_count(store: &Store) -> usize {
    match store
        .execute(&BoundQuery::new("MATCH (e:Episode) RETURN e.id AS id"))
        .expect("count episodes")
    {
        QueryResult::Rows(rows) => rows.row_count(),
        other => panic!("unexpected query result: {other:?}"),
    }
}

/// END-TO-END: build a store on disk, then simulate an OLDER-catalog store by dropping a
/// catalog type that a later schema version added (the v3 `Tag`/`HAS_TAG` work-tracking
/// facet) and winding the recorded version back — both committed to the WAL. Reopening via
/// `Store::open_or_recover` must recreate the missing type (additive, non-lossy), rebuild
/// its catalog indexes, advance the recorded version once the binding is whole again, and
/// leave the pre-existing data intact. Pins the schema-additive slice of the greenfield-tax
/// fix: an additive catalog bump converges on open, with no fresh store.
#[test]
fn recover_creates_a_missing_catalog_type_and_advances_the_version_non_lossily() {
    let dir = temp_dir("recover-missing-type");
    let config = StoreConfig::default(); // a real (large) embedder dimension on disk

    // Write phase: migrate at the current catalog, insert one Episode, then tear off the v3
    // Tag/HAS_TAG facet and wind the recorded version back, so the persisted WAL replays the
    // shape of a store first migrated under an older catalog.
    {
        let store =
            Store::open_persistent_migrated(&dir, config, &now()).expect("open and migrate");
        assert_eq!(
            store.schema_version().expect("version"),
            SCHEMA_VERSION,
            "a freshly migrated store is current"
        );
        let _ = insert_raw_episode(&store, "a recovered episode", "2026-06-06T17:00:00Z");
        assert_eq!(
            episode_count(&store),
            1,
            "the row is present before teardown"
        );

        // Drop the edge first (RESTRICT refuses a node-type drop with an inbound edge-type
        // reference), then the node; both commit to the WAL.
        store
            .execute(&BoundQuery::new("DROP EDGE TYPE IF EXISTS :HAS_TAG"))
            .expect("drop HAS_TAG edge type");
        store
            .execute(&BoundQuery::new("DROP NODE TYPE IF EXISTS :Tag CASCADE"))
            .expect("drop Tag node type");
        store
            .execute(
                &BoundQuery::new("MATCH (v:SchemaVersion) SET v.current_version = $v")
                    .bind("v", Value::Int(SCHEMA_VERSION - 1))
                    .unwrap(),
            )
            .expect("wind the recorded version back");
        assert_eq!(
            store.schema_version().expect("version"),
            SCHEMA_VERSION - 1,
            "the recorded version is wound back"
        );
        // Drop releases the WAL file lock so recovery can reopen it in this process.
        drop(store);
    }

    // Recovery phase: `open_or_recover` runs `reconcile_additive_schema` before the index
    // reconciliation, so the missing type is recreated, its indexes rebuilt, and the
    // recorded version advanced once the binding declares the full catalog again.
    let recovered = Store::open_or_recover(&dir, config, &now()).expect("recover");
    assert_eq!(
        recovered.schema_version().expect("version"),
        SCHEMA_VERSION,
        "the recorded version advances after additive reconciliation"
    );
    // The recreated node type carries its catalog indexes again (`ensure_catalog_indexes`
    // ran after the type existed).
    assert!(
        recovered
            .property_indexes()
            .iter()
            .any(|(label, property)| label == "Tag" && property == "slug"),
        "the recreated type's scalar index is rebuilt"
    );
    // Functional proof the type is usable: a Tag write succeeds — it would fail against an
    // undeclared type.
    recovered
        .execute(
            &BoundQuery::new(
                "INSERT (t:Tag {id: $id, ingested_at: $ts, namespace: $ns, slug: $slug})",
            )
            .bind_uuid("id", tag_id("tag:rust"))
            .unwrap()
            .bind("ts", zdt())
            .unwrap()
            .bind_str("ns", "agent:test")
            .unwrap()
            .bind_str("slug", "rust")
            .unwrap(),
        )
        .expect("insert into the recreated Tag type");
    // Non-lossy: the pre-existing episode survived recovery.
    assert_eq!(
        episode_count(&recovered),
        1,
        "the episode survives recovery (non-lossy)"
    );

    // Idempotent: a second recovery finds nothing missing and keeps the version current.
    drop(recovered);
    let rerecovered = Store::recover(&dir, config, &now()).expect("re-recover");
    assert_eq!(
        rerecovered.schema_version().expect("version"),
        SCHEMA_VERSION,
        "a re-recovered, already-whole store stays current"
    );
    drop(rerecovered);
    let _ = std::fs::remove_dir_all(&dir);
}
