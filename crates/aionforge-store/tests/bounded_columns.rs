//! Bounded `STRING(n)` catalog columns (selene 1.2): an oversize write is rejected at
//! the engine boundary under STRICT, and a short value is stored as its exact text —
//! the max-only `STRING(n)` form, never space-padded like `CHAR(n)` (which would
//! corrupt fixed-width hashes and break enum round-trips).

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_store::{BoundQuery, QueryResult, Store, StoreError, Value};

use jiff::Zoned;

fn now() -> Zoned {
    "2026-06-06T12:00:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn migrated() -> Store {
    Store::open_in_memory_migrated(&now()).expect("open and migrate")
}

fn tag_id(tag: &str) -> Id {
    Id::from_content_hash(tag.as_bytes())
}

/// Insert a minimal valid Fact, binding `object_kind` (a `STRING(32)` column) to the
/// given value so the bound can be exercised independently of the rest of the row.
fn insert_fact_with_object_kind(
    store: &Store,
    id: &str,
    object_kind: &str,
) -> Result<(), StoreError> {
    let ts = Value::ZonedDateTime(Box::new(now()));
    let query = BoundQuery::new(
        "INSERT (f:Fact {id: $id, ingested_at: $ts, namespace: $ns, importance: $imp, \
         trust: $tr, last_access: $ts, access_count_recent: $ac, referenced_count: $rc, \
         surprise: $su, subject_id: $subj, predicate: $pred, object_kind: $ok, \
         confidence: $conf, status: $st, statement: $stmt})",
    )
    .bind_uuid("id", tag_id(id))
    .unwrap()
    .bind("ts", ts)
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
    .bind_uuid("subj", tag_id("entity"))
    .unwrap()
    .bind_str("pred", "relates_to")
    .unwrap()
    .bind_str("ok", object_kind)
    .unwrap()
    .bind("conf", Value::Float(0.9))
    .unwrap()
    .bind_str("st", "active")
    .unwrap()
    .bind_str("stmt", "a canonical statement")
    .unwrap();
    store.execute(&query).map(|_| ())
}

#[test]
fn an_oversize_write_to_a_bounded_column_is_rejected_at_commit() {
    let store = migrated();

    // object_kind is STRING(32); a normal short fixed-vocabulary value commits.
    insert_fact_with_object_kind(&store, "ok-fact", "string").expect("a short object_kind fits");

    // A 40-character object_kind exceeds STRING(32) and is refused at the engine
    // boundary under STRICT, rather than silently stored.
    let oversize = "x".repeat(40);
    let result = insert_fact_with_object_kind(&store, "big-fact", &oversize);
    assert!(
        result.is_err(),
        "an over-length object_kind must be rejected, got {result:?}"
    );
}

#[test]
fn a_short_value_in_a_bounded_column_is_stored_unpadded() {
    let store = migrated();

    // A blake3 content hash is exactly 64 hex chars; consolidation_state 'raw' is 3.
    let content = "a canonical episode";
    let id = tag_id("ep");
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
        captured_at: now(),
        agent_id: tag_id("agent"),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("insert episode");

    // The short consolidation_state ('raw', 3 chars) is stored as its exact text in the
    // STRING(32) column — not space-padded to 32, which CHAR(32) would do (and which
    // would break the enum round-trip on read-back).
    let query = BoundQuery::new(
        "MATCH (e:Episode {id: $id}) RETURN e.consolidation_state AS s, e.content_hash AS h",
    )
    .bind_uuid("id", id)
    .unwrap();
    match store
        .execute(&query)
        .expect("read back the bounded columns")
    {
        QueryResult::Rows(rows) => {
            match rows.value(0, 0) {
                Some(Value::String(state)) => {
                    assert_eq!(state.as_str(), "raw", "short value stored unpadded");
                }
                other => panic!("consolidation_state was not a string: {other:?}"),
            }
            match rows.value(0, 1) {
                Some(Value::String(hash)) => {
                    assert_eq!(
                        hash.as_str().len(),
                        64,
                        "the 64-char hash round-trips exactly"
                    );
                }
                other => panic!("content_hash was not a string: {other:?}"),
            }
        }
        other => panic!("unexpected query result: {other:?}"),
    }
}
