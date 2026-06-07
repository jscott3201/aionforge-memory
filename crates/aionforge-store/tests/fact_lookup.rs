//! Tests for the indexed fact-by-subject lookup (M2.T08 high-precision seed).
//!
//! `Fact.subject_id` is scalar-indexed, so `facts_by_subject` is a probe: it returns
//! every fact about a subject regardless of status, which the high-precision retrieval
//! path composes with the current-support set.

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::semantic::{Fact, FactStatus};
use aionforge_domain::time::Timestamp;
use aionforge_domain::value::ObjectValue;
use aionforge_store::{Store, StoreConfig};

fn ts() -> Timestamp {
    "2026-06-06T09:30:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn store() -> Store {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store.migrate(&ts()).expect("migrate store");
    store
}

fn fact(subject: &Id, predicate: &str, object: &str, status: FactStatus) -> Fact {
    Fact {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts(),
            namespace: Namespace::Global,
            expired_at: None,
        },
        stats: Stats {
            importance: 0.5,
            trust: 0.8,
            last_access: ts(),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.1,
            is_pinned: false,
        },
        subject_id: subject.clone(),
        predicate: predicate.to_string(),
        object: ObjectValue::Text(object.to_string()),
        confidence: 0.9,
        status,
        statement: format!("{predicate} {object}"),
        embedding: None,
        embedder_model: None,
        extraction: None,
    }
}

#[test]
fn facts_by_subject_returns_every_fact_about_the_subject() {
    let store = store();
    let acme = Id::generate();
    let other = Id::generate();

    // Two facts about acme (one superseded — status is irrelevant to the lookup), one
    // about a different subject.
    store
        .insert_fact(&fact(&acme, "based_in", "NYC", FactStatus::Superseded))
        .expect("insert");
    store
        .insert_fact(&fact(&acme, "based_in", "SF", FactStatus::Active))
        .expect("insert");
    store
        .insert_fact(&fact(&other, "based_in", "LA", FactStatus::Active))
        .expect("insert");

    let acme_facts = store.facts_by_subject(&acme).expect("lookup");
    assert_eq!(acme_facts.len(), 2, "both acme facts, regardless of status");

    let other_facts = store.facts_by_subject(&other).expect("lookup");
    assert_eq!(
        other_facts.len(),
        1,
        "only the one fact about the other subject"
    );

    let unknown = store.facts_by_subject(&Id::generate()).expect("lookup");
    assert!(unknown.is_empty(), "an unknown subject has no facts");
}
