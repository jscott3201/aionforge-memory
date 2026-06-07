//! Acceptance tests for the maintained current-state providers (data-model §9, M2.T02).
//!
//! `persistence.rs` and `indexes.rs` already pin provider *registration* and *count*
//! recovery. This file pins the part those cannot: that each provider materializes the
//! *correct nodes* — read back through the typed, generation-checked
//! [`Store::candidate_state_members`] surface and driven by the M2.T01 typed write
//! operations (`assert_fact` / `supersede_fact` / `contradict_fact`), which are how
//! real callers move facts in and out of the current set. The membership rules are
//! pure edge presence, so these tests deliberately read membership independently of the
//! scalar `status` mirror to prove the two agree without one standing in for the other.

use std::collections::BTreeSet;
use std::path::PathBuf;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::edges::{About, Contradicts, SupersededBy};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::value::ObjectValue;
use aionforge_store::{CandidateSet, NodeId, Store, StoreConfig};

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

/// A fresh, empty temp directory unique to `label`, removed first so re-runs start
/// clean. Mirrors the no-temp-crate convention in `persistence.rs`.
fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aionforge-providers-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
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
        aliases: vec![],
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

fn superseded_by(reason: &str, from: &str) -> SupersededBy {
    SupersededBy {
        reason: reason.to_string(),
        temporal: BiTemporal {
            valid_from: ts(from),
            valid_to: None,
            ingested_at: ts(from),
            expired_at: None,
        },
    }
}

fn contradicts(by: &str, from: &str) -> Contradicts {
    Contradicts {
        detected_by: by.to_string(),
        temporal: BiTemporal {
            valid_from: ts(from),
            valid_to: None,
            ingested_at: ts(from),
            expired_at: None,
        },
    }
}

/// The current membership of `set` as a node-id set, via the typed accessor.
fn members(store: &Store, set: CandidateSet) -> BTreeSet<NodeId> {
    store
        .candidate_state_members(set)
        .expect("candidate-state members")
        .into_iter()
        .collect()
}

/// The current `current_support_facts` membership mapped to domain id strings. Domain
/// ids are stable across recovery (node ids are an engine-internal currency), so this
/// is the right key for asserting set identity survives a restart.
fn current_fact_ids(store: &Store) -> BTreeSet<String> {
    store
        .candidate_state_members(CandidateSet::CurrentSupportFacts)
        .expect("members")
        .into_iter()
        .map(|node| {
            store
                .fact_by_node_id(node)
                .expect("read fact")
                .expect("member is a live Fact")
                .identity
                .id
                .as_str()
                .to_owned()
        })
        .collect()
}

/// Assert a fact about a freshly inserted subject entity, returning its node id.
fn assert_about(store: &Store, subject: &Entity, f: &Fact, window: &About) -> NodeId {
    let subject_node = store.insert_entity(subject).expect("insert subject entity");
    store
        .assert_fact(f, subject_node, window)
        .expect("assert fact")
}

#[test]
fn an_asserted_fact_joins_current_support_and_unresolved() {
    let store = store();
    let subject = entity("graphs");
    let f = fact(
        subject.identity.id.clone(),
        "is",
        ObjectValue::Text("useful".to_string()),
        "graphs are useful",
    );
    let node = assert_about(
        &store,
        &subject,
        &f,
        &open_window("2026-06-06T00:00:00Z[UTC]"),
    );

    assert!(
        members(&store, CandidateSet::CurrentSupportFacts).contains(&node),
        "a fresh fact with no supersession or contradiction is current support"
    );
    assert!(
        members(&store, CandidateSet::UnresolvedCurrent).contains(&node),
        "nothing contradicts it, so it is unresolved-current too"
    );
    // The grounded set requires incoming SUPPORTS + outgoing HAS_PROVENANCE, neither of
    // which a bare assert wires — so the fact is current but not yet grounded.
    assert!(
        !members(&store, CandidateSet::ProvenanceCurrentSupportFacts).contains(&node),
        "an ungrounded fact is excluded from provenance_current_support_facts"
    );
}

#[test]
fn supersession_moves_membership_from_the_old_fact_to_the_new() {
    let store = store();
    let subject = entity("capital");
    let subject_node = store.insert_entity(&subject).expect("insert entity");
    let old = fact(
        subject.identity.id.clone(),
        "is",
        ObjectValue::Text("Bonn".to_string()),
        "the capital is Bonn",
    );
    let new = fact(
        subject.identity.id.clone(),
        "is",
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

    // Before supersession both are current support.
    let before = members(&store, CandidateSet::CurrentSupportFacts);
    assert!(before.contains(&old_node) && before.contains(&new_node));

    store
        .supersede_fact(
            old_node,
            new_node,
            &superseded_by("capital moved", "1990-10-03T00:00:00Z[UTC]"),
        )
        .expect("supersede");

    let after = members(&store, CandidateSet::CurrentSupportFacts);
    assert!(
        !after.contains(&old_node),
        "an outgoing SUPERSEDED_BY removes the old fact from current support"
    );
    assert!(
        after.contains(&new_node),
        "the replacement stays current after supersession"
    );
    // The old fact carries no incoming CONTRADICTS, so it remains unresolved-current —
    // superseded is not the same state as contradicted.
    assert!(
        members(&store, CandidateSet::UnresolvedCurrent).contains(&old_node),
        "supersession is not contradiction: the old fact is still unresolved-current"
    );
}

#[test]
fn current_support_and_unresolved_current_are_node_level_duals() {
    // The defining §9 invariant: under `challenger -CONTRADICTS-> incumbent`,
    //   current_support_facts excludes the *source* (outgoing CONTRADICTS),
    //   unresolved_current  excludes the *target* (incoming CONTRADICTS).
    // Quarantine is left off so membership is proven by edge presence alone, not by the
    // scalar status mirror.
    let store = store();
    let subject = entity("temperature");
    let subject_node = store.insert_entity(&subject).expect("insert entity");
    let incumbent = fact(
        subject.identity.id.clone(),
        "is",
        ObjectValue::Text("hot".to_string()),
        "it is hot",
    );
    let challenger = fact(
        subject.identity.id.clone(),
        "is",
        ObjectValue::Text("cold".to_string()),
        "it is cold",
    );
    let incumbent_node = store
        .assert_fact(
            &incumbent,
            subject_node,
            &open_window("2026-06-06T00:00:00Z[UTC]"),
        )
        .expect("assert incumbent");
    let challenger_node = store
        .assert_fact(
            &challenger,
            subject_node,
            &open_window("2026-06-06T00:00:00Z[UTC]"),
        )
        .expect("assert challenger");

    store
        .contradict_fact(
            challenger_node,
            incumbent_node,
            &contradicts("contradiction-rule-v1", "2026-06-06T01:00:00Z[UTC]"),
            false,
        )
        .expect("contradict");

    let current = members(&store, CandidateSet::CurrentSupportFacts);
    let unresolved = members(&store, CandidateSet::UnresolvedCurrent);

    assert!(
        current.contains(&incumbent_node) && !current.contains(&challenger_node),
        "current_support drops the contradiction source, keeps the contested incumbent"
    );
    assert!(
        unresolved.contains(&challenger_node) && !unresolved.contains(&incumbent_node),
        "unresolved_current drops the contradiction target, keeps the challenger"
    );
    // current_support_facts minus unresolved_current is exactly the contested
    // incumbents — the §9 quarantine-reasoning set.
    let contested: BTreeSet<NodeId> = current.difference(&unresolved).copied().collect();
    assert_eq!(
        contested,
        BTreeSet::from([incumbent_node]),
        "the set difference isolates the contested incumbent"
    );
}

#[test]
fn quarantining_a_source_drops_it_from_current_support() {
    let store = store();
    let subject = entity("status");
    let subject_node = store.insert_entity(&subject).expect("insert entity");
    let incumbent = fact(
        subject.identity.id.clone(),
        "is",
        ObjectValue::Text("open".to_string()),
        "the ticket is open",
    );
    let challenger = fact(
        subject.identity.id.clone(),
        "is",
        ObjectValue::Text("closed".to_string()),
        "the ticket is closed",
    );
    let incumbent_node = store
        .assert_fact(
            &incumbent,
            subject_node,
            &open_window("2026-06-06T00:00:00Z[UTC]"),
        )
        .expect("assert incumbent");
    let challenger_node = store
        .assert_fact(
            &challenger,
            subject_node,
            &open_window("2026-06-06T00:00:00Z[UTC]"),
        )
        .expect("assert challenger");

    store
        .contradict_fact(
            challenger_node,
            incumbent_node,
            &contradicts("rule", "2026-06-06T01:00:00Z[UTC]"),
            true,
        )
        .expect("contradict + quarantine");

    assert!(
        !members(&store, CandidateSet::CurrentSupportFacts).contains(&challenger_node),
        "the quarantined source leaves current support (by its outgoing CONTRADICTS)"
    );
    assert_eq!(
        store
            .fact_by_node_id(challenger_node)
            .expect("read")
            .expect("present")
            .status,
        FactStatus::Quarantined,
        "the scalar status mirror agrees with the edge-presence membership",
    );
}

#[test]
fn membership_and_counts_track_the_latest_generation() {
    // Every successful read is generation-checked: the engine binds the set to the same
    // snapshot whose generation it validates the provider against, so a read can never
    // lag the committed graph. This asserts the watermark advances with a commit and the
    // membership and the count surface agree on the same snapshot — the observable shape
    // of "generation checks invalidate stale state" at this layer.
    let store = store();

    let generation = |s: &Store| {
        s.candidate_state_infos()
            .expect("infos")
            .into_iter()
            .find(|info| info.name == "current_support_facts")
            .expect("registered")
            .generation
    };

    let before = generation(&store);
    assert!(
        members(&store, CandidateSet::CurrentSupportFacts).is_empty(),
        "nothing is current on a freshly migrated graph"
    );

    let subject = entity("subject");
    let f = fact(
        subject.identity.id.clone(),
        "is",
        ObjectValue::Text("present".to_string()),
        "the subject is present",
    );
    let node = assert_about(
        &store,
        &subject,
        &f,
        &open_window("2026-06-06T00:00:00Z[UTC]"),
    );

    let after = generation(&store);
    assert!(
        after > before,
        "a commit advances the generation watermark ({before} -> {after})"
    );
    let members_now = members(&store, CandidateSet::CurrentSupportFacts);
    assert!(
        members_now.contains(&node),
        "the read reflects the latest commit, not a stale snapshot"
    );
    let count = store
        .candidate_state_infos()
        .expect("infos")
        .into_iter()
        .find(|info| info.name == "current_support_facts")
        .expect("registered")
        .candidate_count;
    assert_eq!(
        count,
        members_now.len(),
        "the count surface and the membership surface agree on one snapshot"
    );
}

#[test]
fn current_support_membership_rebuilds_at_node_identity_after_recovery() {
    // Strengthens the count-level recovery test in persistence.rs to node identity:
    // the *same facts* are current after a restart, with no parallel index persisted —
    // the membership is rebuilt purely by replaying the primary edges from the WAL.
    let dir = temp_dir("recover-identity");
    let config = StoreConfig {
        embedding_dimension: 4,
    };

    let kept_id;
    let superseded_id;
    {
        let store = Store::open_persistent_migrated(
            &dir,
            config,
            &ts("2026-01-01T00:00:00-06:00[America/Chicago]"),
        )
        .expect("open and migrate");
        let subject = entity("capital");
        let subject_node = store.insert_entity(&subject).expect("insert entity");
        let old = fact(
            subject.identity.id.clone(),
            "is",
            ObjectValue::Text("Bonn".to_string()),
            "the capital is Bonn",
        );
        let new = fact(
            subject.identity.id.clone(),
            "is",
            ObjectValue::Text("Berlin".to_string()),
            "the capital is Berlin",
        );
        superseded_id = old.identity.id.as_str().to_owned();
        kept_id = new.identity.id.as_str().to_owned();
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
        store
            .supersede_fact(
                old_node,
                new_node,
                &superseded_by("capital moved", "1990-10-03T00:00:00Z[UTC]"),
            )
            .expect("supersede");

        assert_eq!(
            current_fact_ids(&store),
            BTreeSet::from([kept_id.clone()]),
            "only the un-superseded fact is current before recovery"
        );
        drop(store);
    }

    let recovered = Store::recover(&dir, config).expect("recover");
    assert_eq!(
        current_fact_ids(&recovered),
        BTreeSet::from([kept_id.clone()]),
        "the same fact is current after recovery, rebuilt from the WAL alone"
    );
    assert!(
        !current_fact_ids(&recovered).contains(&superseded_id),
        "the superseded fact stays out of current support across recovery"
    );
    drop(recovered);
    let _ = std::fs::remove_dir_all(&dir);
}
