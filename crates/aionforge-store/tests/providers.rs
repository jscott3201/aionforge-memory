//! Acceptance tests for the maintained current-state providers (data-model §9, M2.T02).
//!
//! `persistence.rs` and `indexes.rs` already pin provider *registration* and *count*
//! recovery. This file pins the part those cannot: that each provider materializes the
//! *correct nodes* — read back through the typed, generation-checked
//! [`Store::candidate_state_members`] surface and driven by the M2.T01 typed write
//! operations (`assert_fact` / `supersede_fact` / `contradict_fact`), which are how
//! real callers move facts in and out of the current set. The membership rules are
//! pure edge presence, so these tests read membership independently of the scalar
//! `status` mirror to prove the two agree without one standing in for the other. The
//! grounded/scope/recency sets, which need edges no typed writer exists for yet, are in
//! `provider_grounding.rs`; the shared fixtures live in `common/mod.rs`.

mod common;
use common::*;

use std::collections::BTreeSet;

use aionforge_domain::ids::Id;
use aionforge_domain::nodes::semantic::FactStatus;
use aionforge_domain::time::Timestamp;
use aionforge_domain::value::ObjectValue;
use aionforge_store::{CandidateSet, NodeId, Store, StoreConfig};

#[test]
fn an_asserted_fact_joins_current_support_and_unresolved() {
    let store = store();
    let subject = entity("graphs");
    let f = fact(
        subject.identity.id,
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
        subject.identity.id,
        "is",
        ObjectValue::Text("Bonn".to_string()),
        "the capital is Bonn",
    );
    let new = fact(
        subject.identity.id,
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
        subject.identity.id,
        "is",
        ObjectValue::Text("hot".to_string()),
        "it is hot",
    );
    let challenger = fact(
        subject.identity.id,
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
        subject.identity.id,
        "is",
        ObjectValue::Text("open".to_string()),
        "the ticket is open",
    );
    let challenger = fact(
        subject.identity.id,
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
fn current_support_membership_is_edge_driven_not_status_driven() {
    // The provider rule keys on edge presence alone; it cannot read the scalar `status`
    // mirror (§9). So a fact whose status is already `quarantined` but which carries no
    // SUPERSEDED_BY / CONTRADICTS edge is still in current_support_facts — the
    // `status = 'active'` exclusion is the caller's query-time filter, layered on top,
    // not part of the provider. This isolates the two conditions the other tests
    // exercise together.
    let store = store();
    let mut quarantined = fact(
        Id::generate(),
        "is",
        ObjectValue::Text("contested".to_string()),
        "a contested claim with no edges",
    );
    quarantined.status = FactStatus::Quarantined;
    let node = store.insert_fact(&quarantined).expect("insert");

    assert!(
        members(&store, CandidateSet::CurrentSupportFacts).contains(&node),
        "an edge-free fact is in the edge-presence set regardless of its scalar status"
    );
    assert_eq!(
        store
            .fact_by_node_id(node)
            .expect("read")
            .expect("present")
            .status,
        FactStatus::Quarantined,
        "its stored scalar status really is quarantined — the provider just does not read it",
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
        subject.identity.id,
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
            subject.identity.id,
            "is",
            ObjectValue::Text("Bonn".to_string()),
            "the capital is Bonn",
        );
        let new = fact(
            subject.identity.id,
            "is",
            ObjectValue::Text("Berlin".to_string()),
            "the capital is Berlin",
        );
        superseded_id = old.identity.id.to_string();
        kept_id = new.identity.id.to_string();
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

    // No parallel index is persisted: a candidate-state snapshot only ever lives inside
    // a `snapshot.{seq}.snap` file, and this store never snapshots, so the WAL directory
    // holds none. The membership below therefore has nothing to load from — it can only
    // be rebuilt by replaying the primary edges from the WAL.
    let snap_files: Vec<String> = std::fs::read_dir(&dir)
        .expect("read wal dir")
        .filter_map(|entry| {
            entry
                .ok()
                .map(|e| e.file_name().to_string_lossy().into_owned())
        })
        .filter(|name| name.ends_with(".snap"))
        .collect();
    assert!(
        snap_files.is_empty(),
        "no candidate-state snapshot is persisted; found {snap_files:?}"
    );

    let recovered = Store::recover(&dir, config, &Timestamp::now()).expect("recover");
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
