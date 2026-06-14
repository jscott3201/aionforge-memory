//! Acceptance tests for WAL-backed persistence and recovery.
//!
//! Pins the durability contract: a migrated store with data, indexes, and live
//! candidate-state membership comes back identical after the process drops the graph
//! and recovers from the WAL alone — schema, rows, native indexes, and providers all
//! rebuilt from the replayed log. Recovery also re-runs the §13.5 dimension check that
//! the version-guarded migration would skip on an already-current graph.

use std::path::PathBuf;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_store::{
    BoundQuery, QueryResult, SCHEMA_VERSION, Store, StoreConfig, StoreError, Value,
};

use jiff::Zoned;

/// A stable [`Id`] for a string tag, so the synthetic id columns (now UUID-typed) still
/// join the same way: the same tag always maps to the same UUID within and across queries.
fn tag_id(tag: &str) -> Id {
    Id::from_content_hash(tag.as_bytes())
}

fn now() -> Zoned {
    "2026-06-06T12:00:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn zdt() -> Value {
    Value::ZonedDateTime(Box::new(now()))
}

/// A fresh, empty temp directory unique to `label`, removed first so re-runs start
/// clean. No external temp-dir crate, matching the engine's own durable tests.
fn temp_dir(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("aionforge-persist-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[cfg(unix)]
fn mode_of(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .expect("path exists")
        .permissions()
        .mode()
        & 0o777
}

#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .expect("set directory mode");
}

/// Insert a minimal valid Fact (every required field bound; `is_pinned`/`status` ride
/// their schema defaults where applicable).
fn insert_fact(store: &Store, id: &str, subject: &str) {
    let query = BoundQuery::new(
        "INSERT (f:Fact {id: $id, ingested_at: $ts, namespace: $ns, importance: $imp, \
         trust: $tr, last_access: $ts, access_count_recent: $ac, referenced_count: $rc, \
         surprise: $su, subject_id: $subj, predicate: $pred, object_kind: $ok, \
         confidence: $conf, status: $st, statement: $stmt})",
    )
    .bind_uuid("id", tag_id(id))
    .unwrap()
    .bind("ts", zdt())
    .unwrap()
    .bind_str("ns", "agent:test")
    .unwrap()
    .bind("imp", Value::Float(0.5))
    .unwrap()
    .bind("tr", Value::Float(0.5))
    .unwrap()
    .bind("ac", Value::Uint(0))
    .unwrap()
    .bind("rc", Value::Uint(0))
    .unwrap()
    .bind("su", Value::Float(0.0))
    .unwrap()
    .bind_uuid("subj", tag_id(subject))
    .unwrap()
    .bind_str("pred", "relates_to")
    .unwrap()
    .bind_str("ok", "string")
    .unwrap()
    .bind("conf", Value::Float(0.9))
    .unwrap()
    .bind_str("st", "active")
    .unwrap()
    .bind_str("stmt", "a canonical statement")
    .unwrap();
    store.execute(&query).expect("insert fact");
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

/// Commit a minimal `AuditEvent` with a specific `occurred_at`, so recovery has a real
/// zoned-datetime value to rebuild the `occurred_at` index over.
fn commit_audit_at(store: &Store, marker: &str, occurred: &str) {
    let when: Zoned = occurred.parse().expect("valid zoned datetime");
    let event = AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(marker.as_bytes()),
            ingested_at: now(),
            namespace: Namespace::System,
            expired_at: None,
        },
        kind: AuditKind::Promote,
        subject_id: Id::from_content_hash(b"persist-subject"),
        actor_id: Id::from_content_hash(b"substrate"),
        payload: serde_json::json!({ "marker": marker }),
        signature: String::new(),
        occurred_at: when,
    };
    store.commit_audit(&event).expect("commit audit");
}

fn fact_count(store: &Store) -> usize {
    match store
        .execute(&BoundQuery::new("MATCH (f:Fact) RETURN f.id AS id"))
        .expect("count facts")
    {
        QueryResult::Rows(rows) => rows.row_count(),
        other => panic!("unexpected query result: {other:?}"),
    }
}

fn provider_count(store: &Store, name: &str) -> usize {
    store
        .candidate_state_infos()
        .expect("candidate-state infos")
        .into_iter()
        .find(|info| info.name == name)
        .unwrap_or_else(|| panic!("provider {name} is registered"))
        .candidate_count
}

/// The `(reason, target-id)` of the single SUPERSEDED_BY edge out of `from`. Reads the
/// edge's property and orientation back, so a recovered edge is verified directly, not
/// just through its effect on a provider's membership.
fn superseded_by(store: &Store, from: &Id) -> (String, String) {
    let query = BoundQuery::new(
        "MATCH (a:Fact {id: $from})-[r:SUPERSEDED_BY]->(b:Fact) \
         RETURN r.reason AS reason, b.id AS target",
    )
    .bind_uuid("from", from)
    .unwrap();
    match store.execute(&query).expect("query superseded_by edge") {
        QueryResult::Rows(rows) => {
            assert_eq!(
                rows.row_count(),
                1,
                "exactly one SUPERSEDED_BY out of {from}"
            );
            let reason = match rows.value(0, 0) {
                Some(Value::String(value)) => value.as_str().to_owned(),
                other => panic!("reason was not a string: {other:?}"),
            };
            let target = match rows.value(0, 1) {
                Some(Value::Uuid(u)) => u.to_string(),
                other => panic!("target id was not a uuid: {other:?}"),
            };
            (reason, target)
        }
        other => panic!("unexpected query result: {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn open_persistent_creates_owner_only_data_dir() {
    let dir = temp_dir("owner-only-dir");

    let store =
        Store::open_persistent(&dir, StoreConfig::default()).expect("open persistent store");
    assert_eq!(mode_of(&dir), 0o700);
    drop(store);

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn open_persistent_refuses_group_or_other_accessible_data_dir() {
    let dir = temp_dir("loose-dir");
    std::fs::create_dir_all(&dir).expect("create loose dir");
    set_mode(&dir, 0o755);

    let error = Store::open_persistent(&dir, StoreConfig::default())
        .expect_err("loose data directory is refused");
    assert!(
        error.to_string().contains("looser than 0700"),
        "unexpected error: {error}"
    );

    set_mode(&dir, 0o700);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn recover_refuses_group_or_other_accessible_data_dir() {
    let dir = temp_dir("loose-recover-dir");
    let config = StoreConfig::default();

    {
        let store =
            Store::open_persistent_migrated(&dir, config, &now()).expect("open and migrate");
        drop(store);
    }

    set_mode(&dir, 0o755);
    let error = Store::recover(&dir, config).expect_err("loose recovery dir is refused");
    assert!(
        error.to_string().contains("looser than 0700"),
        "unexpected error: {error}"
    );

    set_mode(&dir, 0o700);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn persistence_round_trips_schema_data_indexes_and_providers() {
    let dir = temp_dir("round-trip");
    let config = StoreConfig::default();

    // Write phase: migrate, insert two Facts, supersede one so a provider has state.
    {
        let store =
            Store::open_persistent_migrated(&dir, config, &now()).expect("open and migrate");
        insert_fact(&store, "fact-1", "entity-a");
        insert_fact(&store, "fact-2", "entity-b");
        // A real zoned-datetime row so recovery rebuilds the occurred_at index over actual data.
        commit_audit_at(
            &store,
            "persist-audit",
            "2026-06-06T09:00:00-05:00[America/Chicago]",
        );
        let supersede = BoundQuery::new(
            "MATCH (a:Fact {id: $from}), (b:Fact {id: $to}) \
             INSERT (a)-[:SUPERSEDED_BY {valid_from: $ts, ingested_at: $ts, reason: $reason}]->(b)",
        )
        .bind_uuid("from", tag_id("fact-1"))
        .unwrap()
        .bind_uuid("to", tag_id("fact-2"))
        .unwrap()
        .bind("ts", zdt())
        .unwrap()
        .bind_str("reason", "superseded")
        .unwrap();
        store.execute(&supersede).expect("supersede");

        assert_eq!(
            store.schema_version().expect("schema version"),
            SCHEMA_VERSION
        );
        assert_eq!(store.vector_indexes().len(), 7);
        assert_eq!(store.text_indexes().len(), 5);
        assert_eq!(store.property_indexes().len(), 59);
        assert_eq!(store.composite_indexes().len(), 6);
        assert_eq!(fact_count(&store), 2);
        assert_eq!(provider_count(&store, "current_support_facts"), 1);
        // Drop releases the WAL file lock so recovery can reopen it in this process.
        drop(store);
    }

    // Recovery phase: from the WAL alone, everything must come back identical.
    let recovered = Store::recover(&dir, config).expect("recover");
    assert_eq!(
        recovered.schema_version().expect("schema version"),
        SCHEMA_VERSION,
        "schema version survives"
    );
    assert_eq!(
        recovered.vector_indexes().len(),
        7,
        "vector indexes rebuilt"
    );
    assert_eq!(recovered.text_indexes().len(), 5, "text indexes rebuilt");
    assert_eq!(
        recovered.property_indexes().len(),
        59,
        "property indexes rebuilt"
    );
    assert_eq!(
        recovered.composite_indexes().len(),
        6,
        "composite indexes rebuilt"
    );
    // The first ZONED DATETIME property index in the schema rebuilds into the catalog from the WAL.
    assert!(
        recovered
            .property_indexes()
            .iter()
            .any(|(label, prop)| label == "AuditEvent" && prop == "occurred_at"),
        "AuditEvent.occurred_at datetime index is in the recovered catalog"
    );
    // And it functions over recovered data, not just its name: the persisted zoned-datetime value
    // round-trips through the WAL and a half-open range query against the recovered store finds it.
    let lo: Zoned = "2026-06-06T08:00:00-05:00[America/Chicago]"
        .parse()
        .unwrap();
    let hi: Zoned = "2026-06-06T10:00:00-05:00[America/Chicago]"
        .parse()
        .unwrap();
    let range = BoundQuery::new(
        "MATCH (a:AuditEvent) WHERE a.occurred_at >= $lo AND a.occurred_at < $hi RETURN a.id",
    )
    .bind("lo", Value::ZonedDateTime(Box::new(lo)))
    .unwrap()
    .bind("hi", Value::ZonedDateTime(Box::new(hi)))
    .unwrap();
    match recovered
        .execute(&range)
        .expect("range query over recovered audit")
    {
        QueryResult::Rows(rows) => {
            assert_eq!(
                rows.row_count(),
                1,
                "the recovered audit event is found by occurred_at"
            );
            match rows.value(0, 0) {
                Some(Value::Uuid(u)) => assert_eq!(
                    u.to_string(),
                    Id::from_content_hash(b"persist-audit").to_string(),
                    "the matched id is the persisted audit event"
                ),
                other => panic!("id was not a uuid: {other:?}"),
            }
        }
        other => panic!("unexpected query result: {other:?}"),
    }
    assert_eq!(fact_count(&recovered), 2, "Fact rows survive");
    assert_eq!(
        provider_count(&recovered, "current_support_facts"),
        1,
        "candidate-state membership rebuilt from replayed edges"
    );
    // The edge itself — its property and its direction — survives, not just its effect
    // on the provider count.
    let (reason, target) = superseded_by(&recovered, &tag_id("fact-1"));
    assert_eq!(reason, "superseded", "edge property survives recovery");
    assert_eq!(
        target,
        tag_id("fact-2").to_string(),
        "edge orientation survives recovery"
    );

    // A migrate after recovery is a no-op — the schema is already current.
    assert!(
        recovered.migrate(&now()).expect("re-migrate").is_noop(),
        "post-recovery migrate is a no-op"
    );

    // Post-recovery writes are accepted and durable, proving the WAL was reopened live.
    insert_fact(&recovered, "fact-3", "entity-c");
    assert_eq!(fact_count(&recovered), 3, "post-recovery write lands");
    assert_eq!(
        provider_count(&recovered, "current_support_facts"),
        2,
        "fact-2 and fact-3 are current support after the post-recovery insert"
    );
    drop(recovered);

    let rerecovered = Store::recover(&dir, config).expect("re-recover");
    assert_eq!(
        fact_count(&rerecovered),
        3,
        "the post-recovery write is itself durable"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recovery_runs_the_dimension_consistency_check() {
    let dir = temp_dir("dim-check");
    let written = StoreConfig {
        embedding_dimension: 768,
    };

    {
        let store =
            Store::open_persistent_migrated(&dir, written, &now()).expect("open and migrate");
        assert!(store.vector_indexes().iter().all(|v| v.dimension == 768));
        drop(store);
    }

    // Recovering under a different embedder dimension must fail loudly (§13.5): the
    // recovered indexes are pinned at 768, the asserted dimension is 1536.
    let mismatched = StoreConfig {
        embedding_dimension: 1536,
    };
    assert!(
        Store::recover(&dir, mismatched).is_err(),
        "recovery rejects a dimension mismatch"
    );

    // Recovering under the dimension the indexes were built at succeeds.
    let recovered = Store::recover(&dir, written).expect("recover at the written dimension");
    assert!(
        recovered
            .vector_indexes()
            .iter()
            .all(|v| v.dimension == 768)
    );
    drop(recovered);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recovery_maps_an_unsupported_on_disk_format_to_a_distinct_error() {
    let dir = temp_dir("unsupported-format");
    let config = StoreConfig::default();

    // Write a real, current-format store, then drop it so the WAL file unlocks.
    {
        let store =
            Store::open_persistent_migrated(&dir, config, &now()).expect("open and migrate");
        drop(store);
    }

    // Downgrade the persisted WAL's minor format version on disk to one this build
    // does not read. The 16-byte WAL header is `SLDB` + major(u16-le) + minor(u16-le)
    // + snapshot_seq(u64-le); patching bytes [6..8] flips only the minor version,
    // reusing selene's own writer for the magic and major so the test pins
    // format-version *detection*, not a hand-built header.
    let wal = dir.join(Store::WAL_FILE_NAME);
    let mut bytes = std::fs::read(&wal).expect("read persisted WAL");
    assert_eq!(&bytes[0..4], b"SLDB", "the WAL header magic is stable");
    bytes[6..8].copy_from_slice(&0u16.to_le_bytes());
    std::fs::write(&wal, &bytes).expect("rewrite WAL with an older minor version");

    // Recovery must surface the distinct UnsupportedFormat arm — not an opaque
    // Graph/corruption error — so the runbook can say "recreate fresh", and the
    // message must carry the on-disk version and the actionable guidance.
    let error = Store::recover(&dir, config).expect_err("an older on-disk format is rejected");
    assert!(
        matches!(error, StoreError::UnsupportedFormat { minor: 0, .. }),
        "expected a distinct UnsupportedFormat, got: {error:?}"
    );
    let rendered = error.to_string();
    assert!(
        rendered.contains("unsupported on-disk store format")
            && rendered.contains("recreate it fresh"),
        "the runbook message is actionable and distinct from corruption: {rendered}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_or_recover_creates_then_recovers() {
    let dir = temp_dir("open-or-recover");
    let config = StoreConfig::default();

    // First call: no WAL yet, so it creates the directory, opens fresh, and migrates.
    {
        let store = Store::open_or_recover(&dir, config, &now()).expect("first open creates");
        assert_eq!(
            store.schema_version().expect("schema version"),
            SCHEMA_VERSION
        );
        insert_fact(&store, "fact-1", "entity-a");
        drop(store);
    }

    // Second call: the WAL exists, so it recovers and the data is there.
    let store = Store::open_or_recover(&dir, config, &now()).expect("second open recovers");
    assert_eq!(
        store.schema_version().expect("schema version"),
        SCHEMA_VERSION
    );
    assert_eq!(
        fact_count(&store),
        1,
        "data from the first run is recovered"
    );
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recovered_wal_preserves_episode_event_and_ingestion_time() {
    let dir = temp_dir("episode-timestamps");
    let config = StoreConfig::default();
    let episode_id;

    {
        let store =
            Store::open_persistent_migrated(&dir, config, &now()).expect("open and migrate");
        episode_id =
            insert_raw_episode(&store, "historical captured event", "2026-01-02T03:04:05Z");
        drop(store);
    }

    let recovered = Store::recover(&dir, config).expect("recover");
    let episode = recovered
        .episode_by_id(&episode_id)
        .expect("episode lookup")
        .expect("episode recovered");
    let historical: Zoned = "2026-01-02T03:04:05Z"
        .parse::<jiff::Timestamp>()
        .expect("valid timestamp")
        .to_zoned(jiff::tz::TimeZone::UTC);
    assert_eq!(episode.captured_at, historical);
    assert_eq!(
        episode.identity.ingested_at,
        now(),
        "replay preserves the operational ingestion timestamp separately"
    );
    assert_eq!(
        recovered
            .consolidation_lag()
            .expect("lag")
            .oldest_pending_ingested_at,
        Some(now()),
        "current-format replay drives backlog age from ingestion time"
    );
    drop(recovered);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Insert a minimal valid ProvenanceRecord (every NOT NULL field bound).
fn insert_provenance(store: &Store, id: &str, subject: &str) {
    let query = BoundQuery::new(
        "INSERT (p:ProvenanceRecord {id: $id, ingested_at: $ts, namespace: $ns, \
         subject_id: $subj, writer_agent_id: $writer, signature: $sig, \
         model_family: $mf, trust_at_write: $tw})",
    )
    .bind_uuid("id", tag_id(id))
    .unwrap()
    .bind("ts", zdt())
    .unwrap()
    .bind_str("ns", "agent:test")
    .unwrap()
    .bind_uuid("subj", tag_id(subject))
    .unwrap()
    .bind_uuid("writer", tag_id("agent:test"))
    .unwrap()
    .bind_str("sig", "signature-bytes")
    .unwrap()
    .bind_str("mf", "test-model")
    .unwrap()
    .bind("tw", Value::Float(0.5))
    .unwrap();
    store.execute(&query).expect("insert provenance record");
}

/// Insert a minimal valid Scope (every NOT NULL field bound).
fn insert_scope(store: &Store, id: &str) {
    let query = BoundQuery::new(
        "INSERT (s:Scope {id: $id, ingested_at: $ts, namespace: $ns, name: $name, scope_kind: $kind})",
    )
    .bind_uuid("id", tag_id(id))
    .unwrap()
    .bind("ts", zdt())
    .unwrap()
    .bind_str("ns", "agent:test")
    .unwrap()
    .bind_str("name", "test-scope")
    .unwrap()
    .bind_str("kind", "task")
    .unwrap();
    store.execute(&query).expect("insert scope");
}

/// Insert a minimal valid RecencyWindow (every NOT NULL field bound).
fn insert_recency_window(store: &Store, id: &str) {
    let query = BoundQuery::new(
        "INSERT (w:RecencyWindow {id: $id, ingested_at: $ts, namespace: $ns, label: $label})",
    )
    .bind_uuid("id", tag_id(id))
    .unwrap()
    .bind("ts", zdt())
    .unwrap()
    .bind_str("ns", "agent:test")
    .unwrap()
    .bind_str("label", "last-hour")
    .unwrap();
    store.execute(&query).expect("insert recency window");
}

/// Run a fixed, property-free edge insert binding only the two endpoint ids.
fn insert_edge(store: &Store, source: &str, from: &str, to: &str) {
    let query = BoundQuery::new(source)
        .bind_uuid("from", tag_id(from))
        .unwrap()
        .bind_uuid("to", tag_id(to))
        .unwrap();
    store.execute(&query).expect("insert edge");
}

#[test]
fn candidate_state_providers_survive_recovery() {
    let dir = temp_dir("providers");
    let config = StoreConfig::default();

    // A small graph that places each of the five §9 providers in a known, nonzero state:
    //  - fact-a: plain current fact, and the SUPPORTS source for fact-b.
    //  - fact-b: the grounded incumbent — incoming SUPPORTS + outgoing HAS_PROVENANCE,
    //    in a scope and a recency window, and the target of a CONTRADICTS.
    //  - fact-c: contradicts fact-b (outgoing CONTRADICTS), so it leaves current support.
    let expected = [
        ("current_support_facts", 2usize),       // fact-a, fact-b
        ("provenance_current_support_facts", 1), // fact-b
        ("scope_membership", 1),                 // fact-b
        ("recency_active", 1),                   // fact-b
        ("unresolved_current", 2),               // fact-a, fact-c (fact-b has incoming CONTRADICTS)
    ];

    {
        let store =
            Store::open_persistent_migrated(&dir, config, &now()).expect("open and migrate");
        insert_fact(&store, "fact-a", "entity-a");
        insert_fact(&store, "fact-b", "entity-b");
        insert_fact(&store, "fact-c", "entity-c");
        insert_provenance(&store, "prov-1", "fact-b");
        insert_scope(&store, "scope-1");
        insert_recency_window(&store, "window-1");

        let supports = BoundQuery::new(
            "MATCH (a:Fact {id: $from}), (b:Fact {id: $to}) INSERT (a)-[:SUPPORTS {weight: $w}]->(b)",
        )
        .bind_uuid("from", tag_id("fact-a"))
        .unwrap()
        .bind_uuid("to", tag_id("fact-b"))
        .unwrap()
        .bind("w", Value::Float(1.0))
        .unwrap();
        store.execute(&supports).expect("supports");

        let contradicts = BoundQuery::new(
            "MATCH (a:Fact {id: $from}), (b:Fact {id: $to}) \
             INSERT (a)-[:CONTRADICTS {valid_from: $ts, ingested_at: $ts, detected_by: $by}]->(b)",
        )
        .bind_uuid("from", tag_id("fact-c"))
        .unwrap()
        .bind_uuid("to", tag_id("fact-b"))
        .unwrap()
        .bind("ts", zdt())
        .unwrap()
        .bind_str("by", "contradiction-detector")
        .unwrap();
        store.execute(&contradicts).expect("contradict");

        insert_edge(
            &store,
            "MATCH (a:Fact {id: $from}), (b:ProvenanceRecord {id: $to}) \
             INSERT (a)-[:HAS_PROVENANCE]->(b)",
            "fact-b",
            "prov-1",
        );
        insert_edge(
            &store,
            "MATCH (a:Fact {id: $from}), (b:Scope {id: $to}) INSERT (a)-[:IN_SCOPE]->(b)",
            "fact-b",
            "scope-1",
        );
        insert_edge(
            &store,
            "MATCH (a:Fact {id: $from}), (b:RecencyWindow {id: $to}) INSERT (a)-[:RECENT_IN]->(b)",
            "fact-b",
            "window-1",
        );

        for (name, count) in expected {
            assert_eq!(
                provider_count(&store, name),
                count,
                "{name} before recovery"
            );
        }
        drop(store);
    }

    // Every provider's membership must rebuild from the WAL alone.
    let recovered = Store::recover(&dir, config).expect("recover");
    for (name, count) in expected {
        assert_eq!(
            provider_count(&recovered, name),
            count,
            "{name} rebuilt after recovery"
        );
    }
    drop(recovered);
    let _ = std::fs::remove_dir_all(&dir);
}
