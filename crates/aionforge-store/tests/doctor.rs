//! Doctor-report acceptance for the L0 store health surface.

use aionforge_store::{SCHEMA_VERSION, Store, StoreConfig};

fn now() -> jiff::Zoned {
    "2026-06-06T12:00:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn open_store() -> Store {
    Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store")
}

#[test]
fn unmigrated_store_doctor_reports_unhealthy_without_failing() {
    let store = open_store();

    let report = store.doctor_report().expect("doctor report");

    assert!(!report.ok, "unmigrated store is not healthy");
    assert!(
        report.schema.schema_bound,
        "the graph is still closed/bound"
    );
    assert_eq!(report.schema.current_version, 0);
    assert_eq!(report.schema.target_version, SCHEMA_VERSION);
    assert_eq!(report.schema.node_type_count, 0);
    assert!(
        !report.indexes.ok,
        "expected indexes are missing pre-migration"
    );
    assert!(
        !report.indexes.vector_indexes.missing.is_empty(),
        "missing vector indexes are surfaced"
    );
    assert_eq!(report.capacity.node_count, 0);
    assert_eq!(report.capacity.edge_count, 0);
}

#[test]
fn migrated_store_doctor_reports_current_catalog_and_providers() {
    let store = open_store();
    store.migrate(&now()).expect("migrate store");

    let report = store.doctor_report().expect("doctor report");

    assert!(
        report.ok,
        "fresh migrated store should be healthy: {report:#?}"
    );
    assert!(report.schema.ok);
    assert_eq!(report.schema.current_version, SCHEMA_VERSION);
    assert_eq!(report.schema.target_version, SCHEMA_VERSION);
    assert_eq!(report.schema.node_type_count, 17);
    assert!(report.indexes.ok);
    assert_eq!(report.indexes.expected_embedder_dimension, 4);
    assert!(report.indexes.vector_dimension_mismatches.is_empty());
    assert!(report.indexes.vector_kind_mismatches.is_empty());
    assert_eq!(report.indexes.vector_indexes.actual.len(), 7);
    assert_eq!(report.indexes.text_indexes.actual.len(), 5);
    assert_eq!(report.indexes.property_indexes.actual.len(), 51);
    assert_eq!(report.indexes.composite_indexes.actual.len(), 5);
    assert!(report.providers.ok);
    assert_eq!(report.providers.candidate_state_infos.len(), 5);
    assert_eq!(report.consolidation_lag.episodes_pending, 0);
    assert_eq!(report.consolidation_lag.episodes_failed, 0);
    assert_eq!(
        report.capacity.node_count, 1,
        "migration writes the SchemaVersion singleton"
    );
    assert_eq!(report.capacity.edge_count, 0);
}
