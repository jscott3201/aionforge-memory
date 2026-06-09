//! L0 acceptance for the attestation + quorum-promotion write surface (06 §4, M4.T04).
//!
//! Exercises the store mechanics the orchestrator (L2) drives: a write-when-absent
//! attestation edge, distinct-attester reads, the atomic and idempotent promote/demote
//! write-sets, the content-addressed global-copy id, and the never-destroy-the-original
//! guarantee. The promotion *policy* (posterior, quorum, category rules) lives in L2 and is
//! tested there; here every value is hand-built.

mod common;

use common::{entity, fact, open_window, store, ts};

use aionforge_domain::blocks::Identity;
use aionforge_domain::edges::{About, AttestedBy, DemotedFrom, PromotedTo};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::agent::{Agent, AgentStatus, TrustScores};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind, Promotion, PromotionStatus};
use aionforge_domain::nodes::semantic::{Fact, FactStatus};
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::value::ObjectValue;
use aionforge_store::{NodeId, Store};

const NOW: &str = "2026-06-08T09:00:00-05:00[America/Chicago]";

fn now() -> Timestamp {
    ts(NOW)
}

/// Enroll an agent and return its `(domain id, node id)`.
fn enroll(store: &Store, name: &str) -> (Id, NodeId) {
    let id = Id::generate();
    let agent = Agent {
        identity: Identity {
            id,
            ingested_at: now(),
            namespace: Namespace::Agent(name.to_string()),
            expired_at: None,
        },
        public_key: "cHVibGljLWtleQ==".to_string(),
        model_family: "test".to_string(),
        model_version: None,
        trust_scores: TrustScores::default(),
        status: AgentStatus::Active,
    };
    let node = store.create_agent(&agent).expect("enroll agent");
    (id, node)
}

/// A team fact about a freshly inserted subject entity; returns `(node id, fact)`.
fn team_fact(store: &Store) -> (NodeId, Fact) {
    let subject = entity("graph databases");
    let subject_node = store.insert_entity(&subject).expect("insert subject");
    let f = fact(
        subject.identity.id,
        "preferred_by",
        ObjectValue::Text("the team".to_string()),
        "the team prefers graph databases",
    );
    let node = store
        .assert_fact(&f, subject_node, &open_window(NOW))
        .expect("assert team fact");
    (node, f)
}

fn attested_by(category: Option<&str>, sig: &str) -> AttestedBy {
    AttestedBy {
        attested_at: now(),
        signature: sig.to_string(),
        category: category.map(str::to_string),
    }
}

fn audit(kind: AuditKind, subject: Id, actor: Id, key: &str) -> AuditEvent {
    AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(key.as_bytes()),
            ingested_at: now(),
            namespace: Namespace::System,
            expired_at: None,
        },
        kind,
        subject_id: subject,
        actor_id: actor,
        payload: serde_json::json!({}),
        signature: String::new(),
        occurred_at: now(),
    }
}

fn global_copy(team: &Fact) -> Fact {
    let global_id =
        Id::from_content_hash(format!("global|{}|promoted", team.identity.id).as_bytes());
    let mut global = team.clone();
    global.identity = Identity {
        id: global_id,
        ingested_at: now(),
        namespace: Namespace::Global,
        expired_at: None,
    };
    global
}

fn ledger(candidate: Id, status: PromotionStatus, promoted: Option<Id>) -> Promotion {
    Promotion {
        identity: Identity {
            id: Id::from_content_hash(format!("promotion|{candidate}").as_bytes()),
            ingested_at: now(),
            namespace: Namespace::System,
            expired_at: None,
        },
        candidate_fact_id: candidate,
        posterior: 0.97,
        k: 3,
        status,
        resolved_at: Some(now()),
        promoted_fact_id: promoted,
    }
}

fn lineage() -> BiTemporal {
    BiTemporal {
        valid_from: now(),
        valid_to: None,
        ingested_at: now(),
        expired_at: None,
    }
}

#[test]
fn an_attestation_writes_once_and_is_immutable_on_reattest() {
    let store = store();
    let (fact_node, _) = team_fact(&store);
    let (attester_id, attester_node) = enroll(&store, "ada");

    let first = audit(AuditKind::Attest, attester_id, attester_id, "attest|first");
    store
        .attest_fact(
            fact_node,
            attester_node,
            &attested_by(Some("reliability"), "sig-a"),
            &first,
        )
        .expect("first attestation");

    // A re-attestation by the same agent — even under a different category — does not write a
    // second edge or mutate the immutable signature/instant: the recorded category stays the
    // first one.
    let again = audit(AuditKind::Attest, attester_id, attester_id, "attest|again");
    store
        .attest_fact(
            fact_node,
            attester_node,
            &attested_by(Some("security"), "sig-b"),
            &again,
        )
        .expect("repeat attestation");

    let attesters = store.distinct_attesters(fact_node).expect("attesters");
    assert_eq!(attesters.len(), 1, "one agent attests once");
    assert_eq!(
        attesters[0].category.as_deref(),
        Some("reliability"),
        "the immutable first attestation is preserved, not overwritten"
    );
}

#[test]
fn distinct_attesters_dedup_by_agent_and_carry_their_category() {
    let store = store();
    let (fact_node, _) = team_fact(&store);
    let (ada, ada_node) = enroll(&store, "ada");
    let (bo, bo_node) = enroll(&store, "bo");

    store
        .attest_fact(
            fact_node,
            ada_node,
            &attested_by(Some("reliability"), "s1"),
            &audit(AuditKind::Attest, ada, ada, "a|ada"),
        )
        .expect("ada attests");
    store
        .attest_fact(
            fact_node,
            bo_node,
            &attested_by(None, "s2"),
            &audit(AuditKind::Attest, bo, bo, "a|bo"),
        )
        .expect("bo attests");

    let mut attesters = store.distinct_attesters(fact_node).expect("attesters");
    attesters.sort_by_key(|a| a.attester_id.to_string());
    assert_eq!(attesters.len(), 2, "two distinct attesters");
    let categories: Vec<Option<String>> = {
        let mut by_id = std::collections::BTreeMap::new();
        for a in &attesters {
            by_id.insert(a.attester_id, a.category.clone());
        }
        vec![by_id[&ada].clone(), by_id[&bo].clone()]
    };
    assert_eq!(categories[0].as_deref(), Some("reliability"));
    assert_eq!(
        categories[1], None,
        "an uncategorized attestation reads back as None"
    );
}

#[test]
fn promotion_is_atomic_idempotent_and_preserves_the_original() {
    let store = store();
    let (team_node, team) = team_fact(&store);
    let before = store
        .fact_by_node_id(team_node)
        .expect("read")
        .expect("team fact");

    let global = global_copy(&team);
    let about = About {
        temporal: lineage(),
    };
    let promoted = PromotedTo {
        temporal: lineage(),
    };
    let row = ledger(
        team.identity.id,
        PromotionStatus::Promoted,
        Some(global.identity.id),
    );
    let promote_audit = audit(
        AuditKind::Promote,
        team.identity.id,
        Id::from_content_hash(b"substrate"),
        "promote|1",
    );

    let ids = store
        .promote_fact(team_node, &global, &about, &promoted, &row, &promote_audit)
        .expect("promote");

    // The global copy landed under a distinct, content-addressed id.
    assert_ne!(
        global.identity.id, team.identity.id,
        "global id differs from team id"
    );
    let resolved = store
        .fact_node_by_id(&global.identity.id)
        .expect("probe")
        .expect("global copy exists");
    assert_eq!(resolved, ids.global_fact);
    let stored_global = store
        .fact_by_node_id(resolved)
        .expect("read")
        .expect("global");
    assert_eq!(stored_global.identity.namespace, Namespace::Global);
    assert_eq!(stored_global.status, FactStatus::Active);

    // The ledger records the promotion.
    let entry = store
        .promotion_by_candidate(&team.identity.id)
        .expect("ledger")
        .expect("ledger row");
    assert_eq!(entry.status, PromotionStatus::Promoted);
    assert_eq!(entry.promoted_fact_id, Some(global.identity.id));

    // The Promote audit is present and points at the candidate.
    let a = store
        .audit_event_by_node_id(ids.audit)
        .expect("read audit")
        .expect("promote audit");
    assert_eq!(a.kind, AuditKind::Promote);

    // The team original is byte-identical — promotion never touches it.
    let after = store
        .fact_by_node_id(team_node)
        .expect("read")
        .expect("team fact");
    assert_eq!(before, after, "the namespace original is untouched");

    // A second promotion is a full no-op: same global node, one ledger row.
    let ids2 = store
        .promote_fact(team_node, &global, &about, &promoted, &row, &promote_audit)
        .expect("re-promote");
    assert_eq!(ids2.global_fact, ids.global_fact, "no second global node");
    assert_eq!(
        store
            .promotion_by_candidate(&team.identity.id)
            .unwrap()
            .unwrap()
            .status,
        PromotionStatus::Promoted
    );
}

#[test]
fn demotion_quarantines_the_global_copy_and_leaves_the_original() {
    let store = store();
    let (team_node, team) = team_fact(&store);
    let team_before = store
        .fact_by_node_id(team_node)
        .expect("read")
        .expect("team");

    let global = global_copy(&team);
    let about = About {
        temporal: lineage(),
    };
    let promoted = PromotedTo {
        temporal: lineage(),
    };
    let promote_row = ledger(
        team.identity.id,
        PromotionStatus::Promoted,
        Some(global.identity.id),
    );
    let ids = store
        .promote_fact(
            team_node,
            &global,
            &about,
            &promoted,
            &promote_row,
            &audit(
                AuditKind::Promote,
                team.identity.id,
                Id::from_content_hash(b"sub"),
                "p|1",
            ),
        )
        .expect("promote");
    let global_node = ids.global_fact;

    // Demote on lost support.
    let demoted = DemotedFrom {
        temporal: lineage(),
    };
    let reject_row = ledger(
        team.identity.id,
        PromotionStatus::Rejected,
        Some(global.identity.id),
    );
    store
        .demote_fact(
            global_node,
            team_node,
            &demoted,
            &now(),
            &reject_row,
            &audit(
                AuditKind::Demote,
                global.identity.id,
                Id::from_content_hash(b"sub"),
                "d|1",
            ),
            &audit(
                AuditKind::Quarantine,
                global.identity.id,
                Id::from_content_hash(b"sub"),
                "q|1",
            ),
        )
        .expect("demote");

    // The global copy is quarantined: expired and status-mirrored, so the current-support
    // provider drops it.
    let demoted_global = store
        .fact_by_node_id(global_node)
        .expect("read")
        .expect("global");
    assert!(
        demoted_global.identity.expired_at.is_some(),
        "global copy is expired"
    );
    assert_eq!(demoted_global.status, FactStatus::Quarantined);

    // The ledger flipped to rejected.
    let entry = store
        .promotion_by_candidate(&team.identity.id)
        .unwrap()
        .unwrap();
    assert_eq!(entry.status, PromotionStatus::Rejected);

    // The team original is byte-identical — demotion never touches it.
    let team_after = store
        .fact_by_node_id(team_node)
        .expect("read")
        .expect("team");
    assert_eq!(
        team_before, team_after,
        "the namespace original is untouched"
    );

    // A replayed demotion is a no-op: the copy stays exactly as quarantined.
    store
        .demote_fact(
            global_node,
            team_node,
            &demoted,
            &ts("2026-06-09T09:00:00-05:00[America/Chicago]"),
            &reject_row,
            &audit(
                AuditKind::Demote,
                global.identity.id,
                Id::from_content_hash(b"sub"),
                "d|1",
            ),
            &audit(
                AuditKind::Quarantine,
                global.identity.id,
                Id::from_content_hash(b"sub"),
                "q|1",
            ),
        )
        .expect("re-demote");
    let twice = store
        .fact_by_node_id(global_node)
        .expect("read")
        .expect("global");
    assert_eq!(
        twice.identity.expired_at, demoted_global.identity.expired_at,
        "the quarantine instant is not overwritten on replay"
    );
}
