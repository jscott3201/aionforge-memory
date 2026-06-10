//! End-to-end tests for the automatic D1 reliability-decay sweep (06 §5, M4.T05 PR-E2):
//! `Memory::sweep_reliability_decays` reads committed contradiction-quarantine audit rows off
//! the L0 all-namespaces spine and records the producer decays the host wrappers would
//! otherwise drive by hand — idempotently, behind the existing reliability off-switch, with a
//! host-round-tripped watermark cursor.
//!
//! These tests commit emitter-shaped quarantine rows directly (the cheap path); the round-trip
//! drift guard in `reliability_sweep_e2e.rs` drives the *real* consolidation pipeline —
//! extractor, contradiction detection, scheduler co-commit — so a change to the emitter's
//! payload shape or reason string fails there, not silently in production.

mod common;

use std::sync::Arc;

use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_engine::{D1SweepReport, Memory, MemoryConfig};
use common::*;

#[test]
fn the_sweep_is_inert_when_reliability_is_off() {
    let store = migrated_store();
    let memory = Memory::new(
        Arc::clone(&store),
        FakeEmbedder::new(),
        MemoryConfig::default(),
        &ts(0),
    )
    .expect("memory without reliability");
    let (fact_id, _) = victim(&store, &Namespace::Agent("ops".to_string()), 1);
    commit_contradiction_quarantine(
        &store,
        &fact_id,
        &Namespace::Agent("ops".to_string()),
        "up",
        1,
    );

    let report = memory
        .sweep_reliability_decays(None, 50, &ts(2))
        .expect("sweep");
    assert_eq!(report, D1SweepReport::default(), "off ⇒ inert, log unread");
    assert_eq!(reliability_event_count(&store), 0);
}

#[test]
fn a_contradiction_quarantine_decays_each_producer_once() {
    let store = migrated_store();
    let memory = memory(&store);
    let namespace = Namespace::Agent("ops".to_string());
    // One victim fact, two distinct producers (two source episodes by different agents).
    let (fact_id, ada) = victim(&store, &namespace, 1);
    let bo = enroll(&store);
    produce(&store, &fact_id, bo, 2);
    commit_contradiction_quarantine(&store, &fact_id, &namespace, "up", 1);

    let report = memory
        .sweep_reliability_decays(None, 50, &ts(2))
        .expect("sweep");
    assert_eq!(report.quarantines_scanned, 1);
    assert_eq!(report.decays_recorded, 2, "one decay per distinct producer");
    assert_eq!(report.victims_unresolved, 0);
    assert!(
        report.next.is_some(),
        "a non-empty page reports a watermark"
    );
    for producer in [&ada, &bo] {
        assert!(
            (agent_score(&store, producer).expect("scored") - 1.0 / 3.0).abs() < EPS,
            "one contradiction folds to 1/3"
        );
    }
}

#[test]
fn the_sweep_skips_governance_demotion_quarantines() {
    let store = migrated_store();
    let memory = memory(&store);
    commit_governance_quarantine(&store, 1);

    let report = memory
        .sweep_reliability_decays(None, 50, &ts(2))
        .expect("sweep");
    assert_eq!(
        report.quarantines_scanned, 0,
        "a D2-channel row is not a D1 trigger"
    );
    assert_eq!(report.decays_recorded, 0);
    assert!(
        report.next.is_some(),
        "a skipped row still advances the watermark past itself"
    );
    assert_eq!(reliability_event_count(&store), 0);
}

#[test]
fn multi_survivor_quarantine_of_one_victim_decays_each_producer_once() {
    // One victim contradicted by two different survivors in one episode mints two distinct
    // quarantine rows — but one wrong fact is one failure: the (victim, producer) decay key
    // collapses both rows to a single decay. (A trigger-keyed scheme would decay twice.)
    let store = migrated_store();
    let memory = memory(&store);
    let namespace = Namespace::Agent("ops".to_string());
    let (fact_id, producer) = victim(&store, &namespace, 1);
    commit_contradiction_quarantine(&store, &fact_id, &namespace, "up", 1);
    commit_contradiction_quarantine(&store, &fact_id, &namespace, "sideways", 2);

    let report = memory
        .sweep_reliability_decays(None, 50, &ts(3))
        .expect("sweep");
    assert_eq!(
        report.quarantines_scanned, 2,
        "both rows are genuine D1 triggers"
    );
    assert_eq!(report.decays_recorded, 1, "but one fact is one failure");
    assert!((agent_score(&store, &producer).expect("scored") - 1.0 / 3.0).abs() < EPS);
}

#[test]
fn a_later_recontradiction_of_the_same_victim_does_not_double_decay() {
    // The ratified Fork-3 semantics: the FACT, not the cycle, is the evidence unit. A later
    // sweep over a fresh quarantine row for an already-decayed victim re-derives the same
    // (victim, producer) event id and records nothing new. A rekey to trigger-id semantics
    // breaks this test.
    let store = migrated_store();
    let memory = memory(&store);
    let namespace = Namespace::Agent("ops".to_string());
    let (fact_id, producer) = victim(&store, &namespace, 1);
    commit_contradiction_quarantine(&store, &fact_id, &namespace, "up", 1);
    let first = memory
        .sweep_reliability_decays(None, 50, &ts(2))
        .expect("first sweep");
    assert_eq!(first.decays_recorded, 1);
    let after_first = agent_score(&store, &producer).expect("scored");

    commit_contradiction_quarantine(&store, &fact_id, &namespace, "another", 3);
    let second = memory
        .sweep_reliability_decays(first.next.as_ref(), 50, &ts(4))
        .expect("second sweep");
    assert_eq!(second.quarantines_scanned, 1, "the new row is scanned");
    assert_eq!(
        second.decays_recorded, 0,
        "but the producer already paid for this fact"
    );
    assert!(
        (agent_score(&store, &producer).expect("scored") - after_first).abs() < EPS,
        "the folded score is unchanged"
    );
}

#[test]
fn host_wrapper_and_auto_sweep_converge_to_one_decay() {
    // The decisive no-double-count proof: the host drives the wrapper first, then the sweep
    // re-derives the SAME content-addressed event and dedups — both paths share one key.
    let store = migrated_store();
    let memory = memory(&store);
    let namespace = Namespace::Agent("ops".to_string());
    let (fact_id, producer) = victim(&store, &namespace, 1);
    commit_contradiction_quarantine(&store, &fact_id, &namespace, "up", 1);

    assert_eq!(
        memory
            .record_reliability_decay(&fact_id, &ts(2))
            .expect("host wrapper"),
        1
    );
    let report = memory
        .sweep_reliability_decays(None, 50, &ts(3))
        .expect("sweep");
    assert_eq!(report.quarantines_scanned, 1);
    assert_eq!(
        report.decays_recorded, 0,
        "the wrapper already recorded this decay"
    );
    assert!((agent_score(&store, &producer).expect("scored") - 1.0 / 3.0).abs() < EPS);
    assert_eq!(
        reliability_event_count(&store),
        1,
        "one event row, two paths"
    );
}

#[test]
fn a_full_rescan_is_a_no_op_after_a_first_sweep() {
    let store = migrated_store();
    let memory = memory(&store);
    let namespace = Namespace::Agent("ops".to_string());
    let (fact_a, ada) = victim(&store, &namespace, 1);
    let (fact_b, bo) = victim(&store, &namespace, 2);
    commit_contradiction_quarantine(&store, &fact_a, &namespace, "up", 1);
    commit_contradiction_quarantine(&store, &fact_b, &namespace, "up", 2);

    let first = memory
        .sweep_reliability_decays(None, 50, &ts(3))
        .expect("first");
    assert_eq!(first.decays_recorded, 2);
    let scores = (
        agent_score(&store, &ada).expect("ada"),
        agent_score(&store, &bo).expect("bo"),
    );

    // A host that lost its watermark rescans from the top: every event dedups, every refold
    // converges to the same fold — crash-replay safety as a visible no-op.
    let second = memory
        .sweep_reliability_decays(None, 50, &ts(4))
        .expect("rescan");
    assert_eq!(second.quarantines_scanned, 2);
    assert_eq!(second.decays_recorded, 0);
    assert_eq!(
        (
            agent_score(&store, &ada).expect("ada"),
            agent_score(&store, &bo).expect("bo"),
        ),
        scores,
        "the refolded caches are identical after the rescan"
    );
}

#[test]
fn a_backdated_row_behind_the_watermark_needs_the_full_rescan() {
    // The watermark caveat, pinned: rows order by the host-supplied `occurred_at`, so a clock
    // regression can land a NEW quarantine behind an already-persisted watermark. The
    // incremental resume never sees it; only the documented `after = None` full rescan heals
    // it — which is why a watermark-only host must still rescan occasionally.
    let store = migrated_store();
    let memory = memory(&store);
    let namespace = Namespace::Agent("ops".to_string());
    let (fact_a, ada) = victim(&store, &namespace, 1);
    commit_contradiction_quarantine(&store, &fact_a, &namespace, "up", 5);

    let first = memory
        .sweep_reliability_decays(None, 50, &ts(6))
        .expect("first sweep");
    assert_eq!(first.decays_recorded, 1);
    let watermark = first.next.expect("watermark");

    // The host clock regresses: a new contradiction lands at minute 2, behind minute 5.
    let (fact_b, bo) = victim(&store, &namespace, 2);
    commit_contradiction_quarantine(&store, &fact_b, &namespace, "up", 2);

    let incremental = memory
        .sweep_reliability_decays(Some(&watermark), 50, &ts(7))
        .expect("incremental resume");
    assert_eq!(
        incremental.quarantines_scanned, 0,
        "the backdated row sorts behind the watermark — invisible incrementally"
    );
    assert!(
        (agent_score(&store, &bo).expect("seeded") - 0.95).abs() < EPS,
        "bo still wears the enrollment sentinel: no decay reached the cache"
    );

    let rescan = memory
        .sweep_reliability_decays(None, 50, &ts(8))
        .expect("full rescan");
    assert_eq!(rescan.quarantines_scanned, 2);
    assert_eq!(
        rescan.decays_recorded, 1,
        "only the backdated row is new; ada's dedups"
    );
    assert!((agent_score(&store, &bo).expect("healed") - 1.0 / 3.0).abs() < EPS);
    assert!((agent_score(&store, &ada).expect("ada") - 1.0 / 3.0).abs() < EPS);
}

#[test]
fn a_quarantine_whose_victim_was_purged_is_counted_not_an_error() {
    let store = migrated_store();
    let memory = memory(&store);
    let namespace = Namespace::Agent("ops".to_string());
    // A quarantine row whose subject resolves to no live fact (e.g. hard-purged since).
    let gone = Id::generate();
    commit_contradiction_quarantine(&store, &gone, &namespace, "up", 1);

    let report = memory
        .sweep_reliability_decays(None, 50, &ts(2))
        .expect("sweep must not abort");
    assert_eq!(report.quarantines_scanned, 1);
    assert_eq!(report.victims_unresolved, 1);
    assert_eq!(report.decays_recorded, 0);
}

#[test]
fn a_victim_with_no_producers_records_nothing() {
    let store = migrated_store();
    let memory = memory(&store);
    let namespace = Namespace::Agent("ops".to_string());
    // A live fact with no DERIVED_FROM source — nobody to decay, cleanly.
    let (_, fact_id) = fact(&store, &namespace, "unproduced");
    commit_contradiction_quarantine(&store, &fact_id, &namespace, "up", 1);

    let report = memory
        .sweep_reliability_decays(None, 50, &ts(2))
        .expect("sweep");
    assert_eq!(report.quarantines_scanned, 1);
    assert_eq!(report.victims_unresolved, 0);
    assert_eq!(report.decays_recorded, 0);
}

#[test]
fn the_sweep_reads_team_namespace_quarantines_without_a_principal() {
    // The namespace posture: quarantine audits live in the VICTIM's namespace, and the sweep
    // reads the all-namespaces L0 spine with no principal — a scoped read would have silently
    // skipped this team row and under-penalized.
    let store = migrated_store();
    let memory = memory(&store);
    let team = Namespace::Team("acme".to_string());
    let (fact_id, producer) = victim(&store, &team, 1);
    commit_contradiction_quarantine(&store, &fact_id, &team, "up", 1);

    let report = memory
        .sweep_reliability_decays(None, 50, &ts(2))
        .expect("sweep");
    assert_eq!(report.decays_recorded, 1);
    assert!((agent_score(&store, &producer).expect("scored") - 1.0 / 3.0).abs() < EPS);
}

#[test]
fn the_cursor_resumes_across_pages_without_reprocessing() {
    let store = migrated_store();
    let memory = memory(&store);
    let namespace = Namespace::Agent("ops".to_string());
    let mut producers = Vec::new();
    for seed in 1..=5u128 {
        let (fact_id, producer) = victim(&store, &namespace, seed);
        let minute = u32::try_from(seed).expect("small");
        commit_contradiction_quarantine(&store, &fact_id, &namespace, "up", minute);
        producers.push(producer);
    }

    // Page through with limit 2: 2 + 2 + 1 rows, each page reporting a watermark.
    let mut after: Option<aionforge_engine::AuditCursor> = None;
    let mut total_scanned = 0;
    let mut total_decays = 0;
    let mut pages = 0;
    loop {
        let report = memory
            .sweep_reliability_decays(after.as_ref(), 2, &ts(10))
            .expect("page");
        if report.next.is_none() {
            assert_eq!(
                report.quarantines_scanned, 0,
                "the empty page ends the loop"
            );
            break;
        }
        pages += 1;
        total_scanned += report.quarantines_scanned;
        total_decays += report.decays_recorded;
        after = report.next;
    }
    assert_eq!(pages, 3, "5 rows at limit 2 ⇒ pages of 2, 2, 1");
    // Load-bearing: `quarantines_scanned` increments BEFORE the dedup, so a cross-page
    // reprocessed row pushes this past 5. `total_decays` alone could not catch it — a
    // replayed row dedups to zero created and the total would sit at 5 regardless.
    assert_eq!(total_scanned, 5);
    assert_eq!(
        total_decays, 5,
        "every victim decayed exactly once across pages"
    );
    for producer in &producers {
        assert!((agent_score(&store, producer).expect("scored") - 1.0 / 3.0).abs() < EPS);
    }
}
