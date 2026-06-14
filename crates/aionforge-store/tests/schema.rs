//! Acceptance tests for the schema and the forward-only migration runner.
//!
//! These pin the data-model §13/§14 contract: the migration declares every kind,
//! applying it twice is a no-op, a dry run reports the pending changes without
//! writing, every bi-temporal edge carries the four-timestamp block, and a
//! `ZONED DATETIME` round-trips through the store without losing its zone or its
//! sub-second precision.

use aionforge_store::{PropertyKind, SCHEMA_VERSION, Store, Value};

use jiff::Zoned;

/// A fixed apply time so the tests are deterministic.
fn now() -> Zoned {
    "2026-06-06T12:00:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

/// The 19 node labels the schema must declare (data-model §4; work-tracking facet adds
/// `WorkItem` + `Tag`).
///
/// Hand-transcribed from the spec, deliberately independent of `catalog.rs` — that
/// independence is the point. Deriving this list from the catalog would only prove the
/// catalog equals itself; transcribing the spec catches a kind the catalog dropped,
/// renamed, or duplicated. The full per-property surface is pinned in `schema_mirror.rs`.
const NODE_KINDS: &[&str] = &[
    "Episode",
    "Fact",
    "Entity",
    "Skill",
    "BadPattern",
    "Note",
    "CoreBlock",
    "Agent",
    "Session",
    "ProvenanceRecord",
    "AuditEvent",
    "Promotion",
    "ConsolidationCursor",
    "SchemaVersion",
    "Scope",
    "RecencyWindow",
    "ValidityAnchor",
    "WorkItem",
    "Tag",
];

/// The 19 edge labels the schema must declare (data-model §5; work-tracking facet adds
/// `HAS_TAG`).
const EDGE_KINDS: &[&str] = &[
    "MENTIONS",
    "ABOUT",
    "SUPPORTS",
    "SUPERSEDED_BY",
    "CONTRADICTS",
    "VALID_AT",
    "IN_SCOPE",
    "IN_SESSION",
    "RECENT_IN",
    "DEPENDS_ON",
    "DERIVED_FROM",
    "ATTESTED_BY",
    "PROMOTED_TO",
    "DEMOTED_FROM",
    "HAS_FAILURE",
    "RELATES_TO",
    "HAS_PROVENANCE",
    "AUDIT",
    "HAS_TAG",
];

/// The 8 bi-temporal edges (data-model §5; the `temporal` block in the domain edges).
const BITEMPORAL_EDGES: &[&str] = &[
    "MENTIONS",
    "ABOUT",
    "SUPERSEDED_BY",
    "CONTRADICTS",
    "VALID_AT",
    "PROMOTED_TO",
    "DEMOTED_FROM",
    "RELATES_TO",
];

/// The four-timestamp block carried by every bi-temporal edge.
const BLOCK: &[&str] = &["valid_from", "valid_to", "ingested_at", "expired_at"];

fn sorted(values: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = values.iter().map(|value| (*value).to_string()).collect();
    out.sort();
    out
}

#[test]
fn migration_declares_every_node_and_edge_kind() {
    let store = Store::open_in_memory_migrated(&now()).expect("open and migrate");
    let snapshot = store
        .schema_snapshot()
        .expect("closed graph has a bound type");

    let node_names: Vec<String> = snapshot
        .node_types
        .iter()
        .map(|node| node.name.clone())
        .collect();
    let edge_names: Vec<String> = snapshot
        .edge_types
        .iter()
        .map(|edge| edge.label.clone())
        .collect();

    assert_eq!(node_names.len(), NODE_KINDS.len(), "node kind count");
    assert_eq!(edge_names.len(), EDGE_KINDS.len(), "edge kind count");
    assert_eq!(sorted_owned(&node_names), sorted(NODE_KINDS), "node kinds");
    assert_eq!(sorted_owned(&edge_names), sorted(EDGE_KINDS), "edge kinds");
}

fn sorted_owned(values: &[String]) -> Vec<String> {
    let mut out = values.to_vec();
    out.sort();
    out
}

#[test]
fn applying_the_migration_twice_is_a_noop() {
    let store = Store::open_in_memory().expect("open store");

    let first = store.migrate(&now()).expect("first migrate");
    assert_eq!(first.from_version, 0);
    assert_eq!(first.to_version, SCHEMA_VERSION);
    assert_eq!(
        first.applied.len(),
        NODE_KINDS.len() + EDGE_KINDS.len(),
        "first run creates every kind"
    );
    assert!(!first.is_noop());
    assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);

    let second = store.migrate(&now()).expect("second migrate");
    assert!(second.is_noop(), "second run changed something: {second:?}");
    assert_eq!(second.from_version, SCHEMA_VERSION);
    assert_eq!(second.to_version, SCHEMA_VERSION);
    assert!(second.applied.is_empty());
    assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);

    // The schema is identical after the second run — no duplicated kinds.
    let snapshot = store.schema_snapshot().expect("bound type");
    assert_eq!(snapshot.node_types.len(), NODE_KINDS.len());
    assert_eq!(snapshot.edge_types.len(), EDGE_KINDS.len());
}

#[test]
fn dry_run_lists_pending_then_nothing_after_apply() {
    let store = Store::open_in_memory().expect("open store");

    let plan = store.migration_plan().expect("plan before apply");
    assert_eq!(plan.current_version, 0);
    assert_eq!(plan.target_version, SCHEMA_VERSION);
    assert_eq!(
        plan.pending.len(),
        NODE_KINDS.len() + EDGE_KINDS.len(),
        "every kind is pending on a fresh store"
    );
    assert!(!plan.is_current());
    // A dry run writes nothing.
    assert_eq!(store.schema_version().expect("version"), 0);

    store.migrate(&now()).expect("migrate");

    let after = store.migration_plan().expect("plan after apply");
    assert_eq!(after.current_version, SCHEMA_VERSION);
    assert!(after.is_current(), "nothing pending after apply: {after:?}");
    assert!(after.pending.is_empty());
}

#[test]
fn every_bitemporal_edge_carries_the_four_timestamp_block() {
    let store = Store::open_in_memory_migrated(&now()).expect("open and migrate");
    let snapshot = store.schema_snapshot().expect("bound type");

    for label in BITEMPORAL_EDGES {
        let edge = snapshot
            .edge_type(label)
            .unwrap_or_else(|| panic!("bi-temporal edge {label} is declared"));
        for field in BLOCK {
            let property = edge
                .property(field)
                .unwrap_or_else(|| panic!("{label} carries {field}"));
            assert_eq!(
                property.value_type,
                PropertyKind::ZonedDateTime,
                "{label}.{field} is a zoned datetime"
            );
        }
        // Lower bounds are required; upper bounds are open; the transaction-time
        // lower bound is immutable (data-model §5, §13.3).
        assert!(
            edge.property("valid_from").unwrap().required,
            "{label}.valid_from NOT NULL"
        );
        assert!(
            edge.property("ingested_at").unwrap().required,
            "{label}.ingested_at NOT NULL"
        );
        assert!(
            edge.property("ingested_at").unwrap().immutable,
            "{label}.ingested_at IMMUTABLE"
        );
        assert!(
            !edge.property("valid_to").unwrap().required,
            "{label}.valid_to nullable"
        );
        assert!(
            !edge.property("expired_at").unwrap().required,
            "{label}.expired_at nullable"
        );
    }

    // Non-bi-temporal edges must NOT carry the block — guards against pasting it where
    // it does not belong. SUPPORTS and IN_SCOPE stand in for the property/marker kinds.
    for label in ["SUPPORTS", "IN_SCOPE"] {
        let edge = snapshot.edge_type(label).expect("declared");
        assert!(
            edge.property("valid_from").is_none(),
            "{label} must not carry the bi-temporal block"
        );
    }
}

#[test]
fn zoned_datetime_round_trips_without_precision_loss() {
    // Sub-second precision and a real IANA zone — both must survive. The store binds
    // timestamps as values (never as interpolated literals), so the engine stores the
    // jiff::Zoned verbatim rather than reparsing it.
    let stamp: Zoned = "2026-05-07T12:34:56.123456789-04:00[America/New_York]"
        .parse()
        .expect("nanosecond zoned datetime");

    let store = Store::open_in_memory().expect("open store");

    // Path 1: parameter binding through a projection.
    let query = aionforge_store::BoundQuery::new("RETURN $t AS t")
        .bind("t", Value::ZonedDateTime(Box::new(stamp.clone())))
        .expect("bind timestamp");
    match store.execute(&query).expect("execute") {
        aionforge_store::QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::ZonedDateTime(read)) => {
                assert_eq!(read.to_string(), stamp.to_string(), "bound round-trip");
            }
            other => panic!("expected a zoned datetime, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }

    // Path 2: stored on the SchemaVersion singleton and read back through MATCH.
    store
        .migrate(&stamp)
        .expect("migrate with the nanosecond stamp");
    let read = store
        .execute(&aionforge_store::BoundQuery::new(
            "MATCH (v:SchemaVersion) RETURN v.applied_at AS applied_at",
        ))
        .expect("read applied_at");
    match read {
        aionforge_store::QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::ZonedDateTime(applied)) => {
                assert_eq!(applied.to_string(), stamp.to_string(), "stored round-trip");
            }
            other => panic!("expected a zoned datetime, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}
