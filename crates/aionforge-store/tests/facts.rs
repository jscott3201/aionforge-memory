//! Integration + property tests for bi-temporal fact semantics (M2.T01).
//!
//! Covers the foundational write operations: asserting a fact wires its `ABOUT`
//! validity window; supersession is non-destructive (the prior fact survives, its
//! event-time window closes to the supersession instant, and the window stays
//! ordered); contradiction is non-destructive (both facts survive). The closing of a
//! window under supersession is property-tested over arbitrary ordered instants.

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::edges::{About, Contradicts, SupersededBy};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::value::ObjectValue;
use aionforge_store::{BoundQuery, QueryResult, Store, StoreConfig, Value};
use proptest::prelude::*;

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
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

fn stats() -> Stats {
    Stats {
        importance: 0.5,
        trust: 0.8,
        last_access: ts("2026-06-06T10:00:00-05:00[America/Chicago]"),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

fn identity(id: Id) -> Identity {
    Identity {
        id,
        ingested_at: ts("2026-06-06T09:30:00-05:00[America/Chicago]"),
        namespace: Namespace::Agent("alice".to_string()),
        expired_at: None,
    }
}

fn entity(name: &str) -> Entity {
    Entity {
        identity: identity(Id::generate()),
        stats: stats(),
        canonical_name: name.to_string(),
        entity_type: "Concept".to_string(),
        aliases: vec![format!("{name}-alias")],
        description: None,
        embedding: None,
        embedder_model: None,
        attributes: None,
    }
}

fn fact(subject: Id, predicate: &str, object: ObjectValue, statement: &str) -> Fact {
    Fact {
        identity: identity(Id::generate()),
        stats: stats(),
        subject_id: subject,
        predicate: predicate.to_string(),
        object,
        confidence: 0.9,
        status: FactStatus::Active,
        statement: statement.to_string(),
        embedding: None,
        embedder_model: None,
        extraction: None,
    }
}

/// An open (current, live) validity window starting at `from`.
fn open_window(from: &str) -> About {
    About {
        temporal: BiTemporal {
            valid_from: ts(from),
            valid_to: None,
            ingested_at: ts(from),
            expired_at: None,
        },
    }
}

/// Count edges of `label` from fact `a` to fact `b` (a presence probe via GQL).
fn edge_count(store: &Store, label: &str, a: &Id, b: &Id) -> u64 {
    // gql-ident-ok: `label` is a trusted static relationship name; the ids are bound.
    let query = BoundQuery::new(format!(
        "MATCH (a:Fact)-[r:{label}]->(b:Fact) WHERE a.id = $a AND b.id = $b RETURN count(r) AS n"
    ))
    .bind_str("a", a.as_str())
    .expect("bind a")
    .bind_str("b", b.as_str())
    .expect("bind b");
    match store.execute(&query).expect("count edges") {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Uint(n)) => *n,
            Some(Value::Int(n)) => u64::try_from(*n).unwrap_or(0),
            _ => 0,
        },
        _ => 0,
    }
}

#[test]
fn a_fact_round_trips_through_insert_and_read() {
    let store = store();
    let subject = Id::generate();
    let original = fact(
        subject,
        "prefers",
        ObjectValue::Text("graph databases".to_string()),
        "the user prefers graph databases",
    );

    let node = store.insert_fact(&original).expect("insert");
    let read = store.fact_by_node_id(node).expect("read").expect("present");
    assert_eq!(
        read, original,
        "fact must survive a commit-then-read unchanged"
    );
}

#[test]
fn an_entity_object_fact_round_trips() {
    let store = store();
    let target = entity("rustls");
    let fact = fact(
        Id::generate(),
        "depends_on",
        ObjectValue::Entity(target.identity.id.clone()),
        "the project depends on rustls",
    );
    let node = store.insert_fact(&fact).expect("insert");
    let read = store.fact_by_node_id(node).expect("read").expect("present");
    assert_eq!(
        read.object, fact.object,
        "entity-object splits and reassembles"
    );
}

#[test]
fn assert_fact_wires_the_about_validity_window() {
    let store = store();
    let subject = entity("graphs");
    let subject_node = store.insert_entity(&subject).expect("insert entity");
    let f = fact(
        subject.identity.id.clone(),
        "is_a",
        ObjectValue::Text("data structure".to_string()),
        "a graph is a data structure",
    );
    let window = open_window("2026-06-01T00:00:00-05:00[America/Chicago]");

    let node = store
        .assert_fact(&f, subject_node, &window)
        .expect("assert");
    let read = store
        .fact_about(node)
        .expect("read about")
        .expect("has about");
    assert_eq!(read, window, "the ABOUT edge carries the validity window");
    assert!(read.temporal.is_current(), "an open window is current");
    assert!(read.temporal.windows_ordered());
}

#[test]
fn supersession_preserves_the_prior_fact_and_closes_its_window() {
    let store = store();
    let subject = entity("capital");
    let subject_node = store.insert_entity(&subject).expect("insert entity");

    let old = fact(
        subject.identity.id.clone(),
        "capital_of",
        ObjectValue::Text("Bonn".to_string()),
        "the capital is Bonn",
    );
    let new = fact(
        subject.identity.id.clone(),
        "capital_of",
        ObjectValue::Text("Berlin".to_string()),
        "the capital is Berlin",
    );
    let old_node = store
        .assert_fact(
            &old,
            subject_node,
            &open_window("1949-09-07T00:00:00Z[UTC]"),
        )
        .expect("assert old");
    let new_node = store
        .assert_fact(
            &new,
            subject_node,
            &open_window("1990-10-03T00:00:00Z[UTC]"),
        )
        .expect("assert new");

    let supersede = SupersededBy {
        reason: "capital moved".to_string(),
        temporal: BiTemporal {
            valid_from: ts("1990-10-03T00:00:00Z[UTC]"),
            valid_to: None,
            ingested_at: ts("2026-06-06T09:30:00-05:00[America/Chicago]"),
            expired_at: None,
        },
    };
    store
        .supersede_fact(old_node, new_node, &supersede)
        .expect("supersede");

    // Non-destructive: the prior fact still exists and reads back unchanged in data.
    let old_read = store
        .fact_by_node_id(old_node)
        .expect("read")
        .expect("preserved");
    assert_eq!(old_read.statement, "the capital is Bonn");
    // Its scalar status mirror flips to superseded.
    assert_eq!(old_read.status, FactStatus::Superseded);
    // Its event-time window closes to the supersession instant and stays ordered.
    let old_about = store
        .fact_about(old_node)
        .expect("about")
        .expect("has about");
    assert_eq!(
        old_about.temporal.valid_to,
        Some(ts("1990-10-03T00:00:00Z[UTC]"))
    );
    assert!(old_about.temporal.windows_ordered());
    assert!(
        !old_about.temporal.is_current(),
        "a closed window is not current"
    );
    // The supersession edge exists; the new fact is untouched and current.
    assert_eq!(
        edge_count(&store, "SUPERSEDED_BY", &old.identity.id, &new.identity.id),
        1
    );
    let new_about = store
        .fact_about(new_node)
        .expect("about")
        .expect("has about");
    assert!(
        new_about.temporal.is_current(),
        "the replacement stays current"
    );
    assert_eq!(
        store
            .fact_by_node_id(new_node)
            .expect("read")
            .expect("present")
            .status,
        FactStatus::Active,
    );
}

#[test]
fn contradiction_preserves_both_facts_and_quarantines_the_source() {
    let store = store();
    let subject = entity("temperature");
    let subject_node = store.insert_entity(&subject).expect("insert entity");
    let a = fact(
        subject.identity.id.clone(),
        "is",
        ObjectValue::Text("hot".to_string()),
        "it is hot",
    );
    let b = fact(
        subject.identity.id.clone(),
        "is",
        ObjectValue::Text("cold".to_string()),
        "it is cold",
    );
    let a_node = store
        .assert_fact(&a, subject_node, &open_window("2026-06-06T00:00:00Z[UTC]"))
        .expect("assert a");
    let b_node = store
        .assert_fact(&b, subject_node, &open_window("2026-06-06T00:00:00Z[UTC]"))
        .expect("assert b");

    let contradicts = Contradicts {
        detected_by: "contradiction-rule-v1".to_string(),
        temporal: BiTemporal {
            valid_from: ts("2026-06-06T01:00:00Z[UTC]"),
            valid_to: None,
            ingested_at: ts("2026-06-06T01:00:00Z[UTC]"),
            expired_at: None,
        },
    };
    store
        .contradict_fact(a_node, b_node, &contradicts, true)
        .expect("contradict");

    // Both facts survive (non-destructive).
    assert!(store.fact_by_node_id(a_node).expect("read").is_some());
    assert!(store.fact_by_node_id(b_node).expect("read").is_some());
    // The negative edge exists and the source is quarantined; the target is untouched.
    assert_eq!(
        edge_count(&store, "CONTRADICTS", &a.identity.id, &b.identity.id),
        1
    );
    assert_eq!(
        store
            .fact_by_node_id(a_node)
            .expect("read")
            .expect("present")
            .status,
        FactStatus::Quarantined,
    );
    assert_eq!(
        store
            .fact_by_node_id(b_node)
            .expect("read")
            .expect("present")
            .status,
        FactStatus::Active,
    );
}

#[test]
fn assert_fact_rejects_an_unordered_window() {
    let store = store();
    let subject = entity("thing");
    let subject_node = store.insert_entity(&subject).expect("insert entity");
    let f = fact(
        subject.identity.id.clone(),
        "is",
        ObjectValue::Bool(true),
        "backwards",
    );
    let backwards = About {
        temporal: BiTemporal {
            valid_from: ts("2026-06-06T12:00:00Z[UTC]"),
            valid_to: Some(ts("2026-06-06T06:00:00Z[UTC]")), // before valid_from
            ingested_at: ts("2026-06-06T12:00:00Z[UTC]"),
            expired_at: None,
        },
    };
    let err = store
        .assert_fact(&f, subject_node, &backwards)
        .expect_err("an unordered window must be rejected");
    assert!(
        matches!(err, aionforge_store::StoreError::Invariant(_)),
        "{err}"
    );
}

#[test]
fn supersession_before_the_facts_valid_from_is_rejected() {
    let store = store();
    let subject = entity("subj");
    let subject_node = store.insert_entity(&subject).expect("insert entity");
    let old = fact(
        subject.identity.id.clone(),
        "p",
        ObjectValue::Number(1.0),
        "old",
    );
    let new = fact(
        subject.identity.id.clone(),
        "p",
        ObjectValue::Number(2.0),
        "new",
    );
    let old_node = store
        .assert_fact(
            &old,
            subject_node,
            &open_window("2026-06-06T12:00:00Z[UTC]"),
        )
        .expect("assert old");
    let new_node = store
        .assert_fact(
            &new,
            subject_node,
            &open_window("2026-06-06T12:00:00Z[UTC]"),
        )
        .expect("assert new");

    // Supersession instant BEFORE the old fact's valid_from would close it backwards.
    let backwards = SupersededBy {
        reason: "too early".to_string(),
        temporal: BiTemporal {
            valid_from: ts("2026-06-06T06:00:00Z[UTC]"),
            valid_to: None,
            ingested_at: ts("2026-06-06T12:00:00Z[UTC]"),
            expired_at: None,
        },
    };
    let err = store
        .supersede_fact(old_node, new_node, &backwards)
        .expect_err("a backwards supersession must be rejected");
    assert!(
        matches!(err, aionforge_store::StoreError::Invariant(_)),
        "{err}"
    );
    // Fail-closed: the rejected write published nothing — the old fact stays current.
    let about = store.fact_about(old_node).expect("about").expect("present");
    assert!(
        about.temporal.is_current(),
        "rejected supersession left the window open"
    );
    assert_eq!(
        store
            .fact_by_node_id(old_node)
            .expect("read")
            .expect("present")
            .status,
        FactStatus::Active,
    );
}

#[test]
fn superseding_a_fact_with_no_about_edge_errors() {
    let store = store();
    // insert_fact bypasses assert_fact, so this fact has no ABOUT edge.
    let old = fact(Id::generate(), "p", ObjectValue::Number(1.0), "old");
    let new = fact(Id::generate(), "p", ObjectValue::Number(2.0), "new");
    let old_node = store.insert_fact(&old).expect("insert old");
    let new_node = store.insert_fact(&new).expect("insert new");
    let supersede = SupersededBy {
        reason: "r".to_string(),
        temporal: BiTemporal {
            valid_from: ts("2026-06-06T12:00:00Z[UTC]"),
            valid_to: None,
            ingested_at: ts("2026-06-06T12:00:00Z[UTC]"),
            expired_at: None,
        },
    };
    let err = store
        .supersede_fact(old_node, new_node, &supersede)
        .expect_err("a fact with no ABOUT edge cannot be superseded");
    assert!(
        matches!(err, aionforge_store::StoreError::Decode(_)),
        "{err}"
    );
}

#[test]
fn contradiction_without_quarantine_keeps_the_source_active() {
    let store = store();
    let subject = entity("subj");
    let subject_node = store.insert_entity(&subject).expect("insert entity");
    let a = fact(
        subject.identity.id.clone(),
        "is",
        ObjectValue::Text("x".into()),
        "a",
    );
    let b = fact(
        subject.identity.id.clone(),
        "is",
        ObjectValue::Text("y".into()),
        "b",
    );
    let a_node = store
        .assert_fact(&a, subject_node, &open_window("2026-06-06T00:00:00Z[UTC]"))
        .expect("assert a");
    let b_node = store
        .assert_fact(&b, subject_node, &open_window("2026-06-06T00:00:00Z[UTC]"))
        .expect("assert b");
    let contradicts = Contradicts {
        detected_by: "d".to_string(),
        temporal: BiTemporal {
            valid_from: ts("2026-06-06T01:00:00Z[UTC]"),
            valid_to: None,
            ingested_at: ts("2026-06-06T01:00:00Z[UTC]"),
            expired_at: None,
        },
    };
    store
        .contradict_fact(a_node, b_node, &contradicts, false)
        .expect("contradict");
    // The negative edge exists but neither fact's status changed.
    assert_eq!(
        edge_count(&store, "CONTRADICTS", &a.identity.id, &b.identity.id),
        1
    );
    assert_eq!(
        store
            .fact_by_node_id(a_node)
            .expect("read")
            .expect("present")
            .status,
        FactStatus::Active,
    );
}

#[test]
fn a_supersession_chain_closes_each_window_in_order() {
    let store = store();
    let subject = entity("capital");
    let subject_node = store.insert_entity(&subject).expect("insert entity");
    let t0 = "1900-01-01T00:00:00Z[UTC]";
    let t1 = "1949-09-07T00:00:00Z[UTC]";
    let t2 = "1990-10-03T00:00:00Z[UTC]";

    let a = fact(
        subject.identity.id.clone(),
        "c",
        ObjectValue::Text("A".into()),
        "A",
    );
    let b = fact(
        subject.identity.id.clone(),
        "c",
        ObjectValue::Text("B".into()),
        "B",
    );
    let c = fact(
        subject.identity.id.clone(),
        "c",
        ObjectValue::Text("C".into()),
        "C",
    );
    let a_node = store
        .assert_fact(&a, subject_node, &open_window(t0))
        .expect("a");
    let b_node = store
        .assert_fact(&b, subject_node, &open_window(t1))
        .expect("b");
    let c_node = store
        .assert_fact(&c, subject_node, &open_window(t2))
        .expect("c");

    let sup = |from: &str| SupersededBy {
        reason: "moved".to_string(),
        temporal: BiTemporal {
            valid_from: ts(from),
            valid_to: None,
            ingested_at: ts("2026-06-06T12:00:00Z[UTC]"),
            expired_at: None,
        },
    };
    store
        .supersede_fact(a_node, b_node, &sup(t1))
        .expect("a->b");
    store
        .supersede_fact(b_node, c_node, &sup(t2))
        .expect("b->c");

    // Each prior window closes to its successor's start; all stay ordered; all preserved.
    let a_about = store.fact_about(a_node).expect("about").expect("a present");
    let b_about = store.fact_about(b_node).expect("about").expect("b present");
    let c_about = store.fact_about(c_node).expect("about").expect("c present");
    assert_eq!(a_about.temporal.valid_to, Some(ts(t1)));
    assert_eq!(b_about.temporal.valid_to, Some(ts(t2)));
    assert!(a_about.temporal.windows_ordered() && b_about.temporal.windows_ordered());
    assert!(
        c_about.temporal.is_current(),
        "the head of the chain stays current"
    );
    assert_eq!(
        edge_count(&store, "SUPERSEDED_BY", &a.identity.id, &b.identity.id),
        1
    );
    assert_eq!(
        edge_count(&store, "SUPERSEDED_BY", &b.identity.id, &c.identity.id),
        1
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// For any supersession instant at or after the fact's `valid_from`, the closed
    /// event-time window is ordered, `valid_to` equals the supersession instant, and
    /// the prior fact is preserved — the bi-temporal supersession invariant.
    #[test]
    fn supersession_closes_an_ordered_window(offset_secs in 0i64..=2_000_000_000i64) {
        let store = store();
        let subject = entity("subject");
        let subject_node = store.insert_entity(&subject).expect("insert entity");

        let from = jiff::Timestamp::new(1_000_000_000, 0)
            .expect("base instant")
            .to_zoned(jiff::tz::TimeZone::UTC);
        let at = jiff::Timestamp::new(1_000_000_000 + offset_secs, 0)
            .expect("supersession instant")
            .to_zoned(jiff::tz::TimeZone::UTC);

        let old = fact(
            subject.identity.id.clone(),
            "p",
            ObjectValue::Number(1.0),
            "old",
        );
        let new = fact(subject.identity.id.clone(), "p", ObjectValue::Number(2.0), "new");
        let about = About {
            temporal: BiTemporal {
                valid_from: from.clone(),
                valid_to: None,
                ingested_at: from.clone(),
                expired_at: None,
            },
        };
        let old_node = store.assert_fact(&old, subject_node, &about).expect("assert old");
        let new_node = store.assert_fact(&new, subject_node, &about).expect("assert new");

        let supersede = SupersededBy {
            reason: "r".to_string(),
            temporal: BiTemporal {
                valid_from: at.clone(),
                valid_to: None,
                ingested_at: at.clone(),
                expired_at: None,
            },
        };
        store.supersede_fact(old_node, new_node, &supersede).expect("supersede");

        let closed = store.fact_about(old_node).expect("about").expect("present");
        prop_assert_eq!(closed.temporal.valid_to.as_ref(), Some(&at));
        prop_assert!(closed.temporal.windows_ordered());
        prop_assert!(store.fact_by_node_id(old_node).expect("read").is_some());
    }
}
