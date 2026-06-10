//! Review-hardening acceptance for the core-block edit gate (05 §4, M5.T04, the PR-3
//! adversarial review riders): a vote authorizes only the exact transition it signed
//! (the content-swap attack refused); `k > 1` counts distinct verified attesters with
//! duplicates collapsed; multi-axis strictness (sensitivity rule + redline flag)
//! composes through actual edits; every rejection reason lands an audit row carrying
//! the refused transition's hashes; a purged block answers `NotFound` through the
//! gate; and an exact replay of an applied no-op edit converges to one audit row.

mod common;

use std::collections::BTreeSet;

use aionforge_domain::authz::Principal;
use aionforge_domain::blocks::Identity;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::nodes::agent::AgentStatus;
use aionforge_domain::nodes::core::BlockKind;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_trust::{
    CoreAttesterVote, CoreEditOutcome, CoreEditPolicy, CoreEditRejection, CoreEditRule,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use common::*;

#[test]
fn a_vote_for_one_transition_does_not_authorize_another() {
    let store = store();
    let (editor_id, _) = enroll(&store, 1, AgentStatus::Active);
    let (attester_id, attester_key) = enroll(&store, 2, AgentStatus::Active);
    let principal = Principal::agent(editor_id);
    let b = block(
        "I disclose conflicts of interest.",
        BlockKind::Commitment,
        None,
    );
    genesis(&store, &b);
    let core = editor(&store, CoreEditPolicy::default(), false);

    // The attester reviewed and vouched for one replacement; the editor ships a
    // different one under the same vote. The transition is in the signed bytes, so
    // this is a forged voucher, not a valid edit with substituted content.
    let reviewed = vote_for(
        &b,
        "I disclose conflicts of interest promptly.",
        &attester_id,
        &attester_key,
    );
    let outcome = core
        .edit(
            &principal,
            &AllowAll,
            &request(&b, "I need not disclose conflicts.", vec![reviewed]),
        )
        .expect("call");
    assert_eq!(
        outcome,
        CoreEditOutcome::Rejected(CoreEditRejection::AttestationFailed)
    );
    assert_eq!(
        store
            .core_block_by_id(&b.identity.id)
            .expect("read")
            .expect("present")
            .content,
        "I disclose conflicts of interest.",
        "the swapped-content edit touched nothing"
    );
}

#[test]
fn a_k_of_two_needs_two_distinct_verified_attesters() {
    let store = store();
    let (editor_id, _) = enroll(&store, 1, AgentStatus::Active);
    let (first_id, first_key) = enroll(&store, 2, AgentStatus::Active);
    let (second_id, second_key) = enroll(&store, 3, AgentStatus::Active);
    let principal = Principal::agent(editor_id);
    let b = block(
        "I am cautious with credentials.",
        BlockKind::Commitment,
        None,
    );
    genesis(&store, &b);
    let core = editor(
        &store,
        CoreEditPolicy {
            default_rule: CoreEditRule {
                k: 2,
                require_human: false,
            },
            ..CoreEditPolicy::default()
        },
        false,
    );

    // One attester voting twice is one attester: duplicates collapse before the count.
    let outcome = core
        .edit(
            &principal,
            &AllowAll,
            &request(
                &b,
                "I never paste credentials.",
                vec![
                    vote_for(&b, "I never paste credentials.", &first_id, &first_key),
                    vote_for(&b, "I never paste credentials.", &first_id, &first_key),
                ],
            ),
        )
        .expect("call");
    assert_eq!(
        outcome,
        CoreEditOutcome::Rejected(CoreEditRejection::InsufficientAttesters {
            required: 2,
            verified: 1,
        })
    );

    // Two distinct verified attesters clear the bar, and both votes are recorded.
    let outcome = core
        .edit(
            &principal,
            &AllowAll,
            &request(
                &b,
                "I never paste credentials.",
                vec![
                    vote_for(&b, "I never paste credentials.", &first_id, &first_key),
                    vote_for(&b, "I never paste credentials.", &second_id, &second_key),
                ],
            ),
        )
        .expect("call");
    let CoreEditOutcome::Applied(receipt) = outcome else {
        panic!("expected Applied, got {outcome:?}");
    };
    assert_eq!(receipt.attesters_recorded, 2);
}

#[test]
fn multi_axis_strictness_is_enforced_through_the_gate() {
    let store = store();
    let (editor_id, _) = enroll(&store, 1, AgentStatus::Active);
    let (model_id, model_key) = enroll(&store, 2, AgentStatus::Active);
    let (human_id, human_key) = enroll(&store, 3, AgentStatus::Active);
    let principal = Principal::agent(editor_id);
    // A redline block carrying a sensitivity: the "pii" rule raises k to 2, the
    // redline flag adds the human requirement — strictest per axis, composed.
    let b = block(
        "I never exfiltrate user data.",
        BlockKind::Redline,
        Some("pii"),
    );
    genesis(&store, &b);
    let mut policy = CoreEditPolicy {
        redline_requires_human: true,
        ..CoreEditPolicy::default()
    };
    policy.rules.insert(
        "pii".to_string(),
        CoreEditRule {
            k: 2,
            require_human: false,
        },
    );
    policy.human_attester_ids = BTreeSet::from([human_id]);
    let core = editor(&store, policy, false);
    let revision = "I never exfiltrate user data, and I say so when asked.";

    // The human alone satisfies the human axis but not the composed k of 2.
    let outcome = core
        .edit(
            &principal,
            &AllowAll,
            &request(
                &b,
                revision,
                vec![vote_for(&b, revision, &human_id, &human_key)],
            ),
        )
        .expect("call");
    assert_eq!(
        outcome,
        CoreEditOutcome::Rejected(CoreEditRejection::InsufficientAttesters {
            required: 2,
            verified: 1,
        })
    );

    // Two model attesters satisfy k but not the redline's human axis.
    let (second_model_id, second_model_key) = enroll(&store, 4, AgentStatus::Active);
    let outcome = core
        .edit(
            &principal,
            &AllowAll,
            &request(
                &b,
                revision,
                vec![
                    vote_for(&b, revision, &model_id, &model_key),
                    vote_for(&b, revision, &second_model_id, &second_model_key),
                ],
            ),
        )
        .expect("call");
    assert_eq!(
        outcome,
        CoreEditOutcome::Rejected(CoreEditRejection::HumanAttestationRequired)
    );

    // A human plus a model clears both axes; the audit pins the composed bar.
    let outcome = core
        .edit(
            &principal,
            &AllowAll,
            &request(
                &b,
                revision,
                vec![
                    vote_for(&b, revision, &human_id, &human_key),
                    vote_for(&b, revision, &model_id, &model_key),
                ],
            ),
        )
        .expect("call");
    assert!(
        matches!(outcome, CoreEditOutcome::Applied(_)),
        "{outcome:?}"
    );
    let rows = core_edit_rows(&store);
    let applied = rows
        .iter()
        .find(|row| row.payload["outcome"] == "applied")
        .expect("applied row");
    assert_eq!(applied.payload["k_required"], 2);
    assert_eq!(applied.payload["require_human"], true);
    assert_eq!(
        applied.payload["human_attester_id"],
        serde_json::json!(human_id.to_string())
    );
}

#[test]
fn every_rejection_reason_is_audited_with_the_refused_transition() {
    let store = store();
    let (editor_id, _) = enroll(&store, 1, AgentStatus::Active);
    let (model_id, model_key) = enroll(&store, 2, AgentStatus::Active);
    let (forger_id, _) = enroll(&store, 3, AgentStatus::Active);
    let principal = Principal::agent(editor_id);

    // One block per reason, so each rejection row stands alone.
    let unsigned_block = block("stance one", BlockKind::Persona, None);
    let forged_block = block("stance two", BlockKind::Persona, None);
    let alone_block = block("stance three", BlockKind::Persona, None);
    let human_block = block("stance four", BlockKind::Persona, Some("pii"));
    for b in [&unsigned_block, &forged_block, &alone_block, &human_block] {
        genesis(&store, b);
    }

    // editor_unverified: signed writes on, no editor signature presented.
    let signed = editor(&store, CoreEditPolicy::default(), true);
    let outcome = signed
        .edit(
            &principal,
            &AllowAll,
            &request(
                &unsigned_block,
                "revised stance one",
                vec![vote_for(
                    &unsigned_block,
                    "revised stance one",
                    &model_id,
                    &model_key,
                )],
            ),
        )
        .expect("call");
    assert_eq!(
        outcome,
        CoreEditOutcome::Rejected(CoreEditRejection::EditorUnverified)
    );

    // attestation_failed: a forged voucher in the set.
    let mut policy = CoreEditPolicy::default();
    policy.rules.insert(
        "pii".to_string(),
        CoreEditRule {
            k: 1,
            require_human: true,
        },
    );
    policy.human_attester_ids = BTreeSet::from([Id::from_content_hash(b"some-human")]);
    let core = editor(&store, policy, false);
    let outcome = core
        .edit(
            &principal,
            &AllowAll,
            &request(
                &forged_block,
                "revised stance two",
                vec![CoreAttesterVote {
                    attester_id: forger_id,
                    attested_at: now(),
                    signature_b64: BASE64.encode([7u8; 64]),
                    category: None,
                }],
            ),
        )
        .expect("call");
    assert_eq!(
        outcome,
        CoreEditOutcome::Rejected(CoreEditRejection::AttestationFailed)
    );

    // insufficient_attesters: a single-writer self-edit.
    let outcome = core
        .edit(
            &principal,
            &AllowAll,
            &request(&alone_block, "revised stance three", vec![]),
        )
        .expect("call");
    assert!(matches!(
        outcome,
        CoreEditOutcome::Rejected(CoreEditRejection::InsufficientAttesters { .. })
    ));

    // human_attestation_required: the pii rule wants a certified human.
    let outcome = core
        .edit(
            &principal,
            &AllowAll,
            &request(
                &human_block,
                "revised stance four",
                vec![vote_for(
                    &human_block,
                    "revised stance four",
                    &model_id,
                    &model_key,
                )],
            ),
        )
        .expect("call");
    assert_eq!(
        outcome,
        CoreEditOutcome::Rejected(CoreEditRejection::HumanAttestationRequired)
    );

    // Every refusal is a row: reason scalar, the editor as actor, and the refused
    // transition's hashes — the same forensic anchors as an applied row.
    let rows = core_edit_rows(&store);
    for (block, reason, revision) in [
        (&unsigned_block, "editor_unverified", "revised stance one"),
        (&forged_block, "attestation_failed", "revised stance two"),
        (
            &alone_block,
            "insufficient_attesters",
            "revised stance three",
        ),
        (
            &human_block,
            "human_attestation_required",
            "revised stance four",
        ),
    ] {
        let row = rows
            .iter()
            .find(|row| row.subject_id == block.identity.id && row.payload["outcome"] == "rejected")
            .unwrap_or_else(|| panic!("a rejection row for {reason}"));
        assert_eq!(row.payload["reason"], reason);
        assert_eq!(row.actor_id, editor_id);
        assert_eq!(row.identity.namespace, block.identity.namespace);
        assert_eq!(
            row.payload["expected_prior_hash"],
            ContentHash::of(block.content.as_bytes()).as_str()
        );
        assert_eq!(
            row.payload["new_content_hash"],
            ContentHash::of(revision.as_bytes()).as_str()
        );
    }
    let shortfall = rows
        .iter()
        .find(|row| row.payload["reason"] == "insufficient_attesters")
        .expect("shortfall row");
    assert_eq!(shortfall.payload["k_required"], 1);
    assert_eq!(shortfall.payload["verified"], 0);
}

#[test]
fn a_purged_block_is_not_found_through_the_gate() {
    let store = store();
    let (editor_id, _) = enroll(&store, 1, AgentStatus::Active);
    let (attester_id, attester_key) = enroll(&store, 2, AgentStatus::Active);
    let principal = Principal::agent(editor_id);
    let b = block("an erased identity", BlockKind::Persona, None);
    genesis(&store, &b);
    let vote = vote_for(&b, "must not apply", &attester_id, &attester_key);

    let node = store
        .memory_by_id(&b.identity.id, &["CoreBlock"])
        .expect("resolve")
        .expect("present")
        .node;
    let purge_audit = AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(b"purge-row"),
            ingested_at: now(),
            namespace: b.identity.namespace.clone(),
            expired_at: None,
        },
        kind: AuditKind::Purge,
        subject_id: b.identity.id,
        actor_id: editor_id,
        payload: serde_json::json!({}),
        signature: String::new(),
        occurred_at: now(),
    };
    store.hard_purge(&[node], &purge_audit).expect("purge");

    let core = editor(&store, CoreEditPolicy::default(), false);
    let outcome = core
        .edit(
            &principal,
            &AllowAll,
            &request(&b, "must not apply", vec![vote]),
        )
        .expect("call");
    assert_eq!(outcome, CoreEditOutcome::NotFound);
}

#[test]
fn an_exact_replay_of_a_noop_edit_converges_to_one_audit_row() {
    let store = store();
    let (editor_id, _) = enroll(&store, 1, AgentStatus::Active);
    let (attester_id, attester_key) = enroll(&store, 2, AgentStatus::Active);
    let principal = Principal::agent(editor_id);
    let b = block("I hold this stance.", BlockKind::Persona, None);
    genesis(&store, &b);
    let core = editor(&store, CoreEditPolicy::default(), false);

    // A no-op transition (prior == new) is the one case the compare-and-swap cannot
    // tell a replay from a first apply: the precondition still holds after the apply.
    // The content-addressed audit id is what makes the at-least-once retry converge.
    let replay = request(
        &b,
        "I hold this stance.",
        vec![vote_for(
            &b,
            "I hold this stance.",
            &attester_id,
            &attester_key,
        )],
    );
    let CoreEditOutcome::Applied(first) = core.edit(&principal, &AllowAll, &replay).expect("call")
    else {
        panic!("first apply");
    };
    let CoreEditOutcome::Applied(second) = core.edit(&principal, &AllowAll, &replay).expect("call")
    else {
        panic!("replayed apply");
    };
    assert_eq!(first.audit_id, second.audit_id, "one verdict, one row");
    assert_eq!(second.attesters_recorded, 1, "the vote edge deduped too");

    let rows = core_edit_rows(&store);
    let applied: Vec<_> = rows
        .iter()
        .filter(|row| row.payload["outcome"] == "applied")
        .collect();
    assert_eq!(applied.len(), 1, "the replayed verdict wrote no second row");
}

#[test]
fn an_editor_without_namespace_authority_is_refused_no_matter_the_votes() {
    use aionforge_domain::authz::DefaultAuthorizer;
    use aionforge_domain::namespace::Namespace;

    let store = store();
    let (outsider_id, _) = enroll(&store, 1, AgentStatus::Active);
    let (attester_id, attester_key) = enroll(&store, 2, AgentStatus::Active);
    let outsider = Principal::agent(outsider_id);
    // The block lives in "identity-owner"'s private namespace; the outsider brings a
    // perfectly valid attester vote anyway.
    let b = block("someone else's identity", BlockKind::Persona, None);
    genesis(&store, &b);
    let core = editor(&store, CoreEditPolicy::default(), false);

    let outcome = core
        .edit(
            &outsider,
            &DefaultAuthorizer,
            &request(
                &b,
                "rewritten by an outsider",
                vec![vote_for(
                    &b,
                    "rewritten by an outsider",
                    &attester_id,
                    &attester_key,
                )],
            ),
        )
        .expect("call");
    // The block sits outside the outsider's visible set, so the refusal answers
    // exactly like an absent id — the edit surface is no existence oracle. The
    // attempt is still audited below.
    assert_eq!(
        outcome,
        CoreEditOutcome::NotFound,
        "attesters vouch for content, never for authority — and an invisible \
         block stays invisible"
    );
    assert_eq!(
        store
            .core_block_by_id(&b.identity.id)
            .expect("read")
            .expect("present")
            .content,
        "someone else's identity"
    );

    // The attempt is on the record under the one cross-namespace kind, in `system`.
    let rows = store
        .audit_by_kind(AuditKind::NamespaceDenied, None, 10)
        .expect("audit")
        .events;
    assert_eq!(rows.len(), 1, "the denial is audited");
    assert_eq!(rows[0].subject_id, b.identity.id);
    assert_eq!(rows[0].actor_id, outsider_id);
    assert_eq!(rows[0].payload["surface"], "core_block_edit");
    assert_eq!(rows[0].identity.namespace, Namespace::System);
}

#[test]
fn a_visible_but_unwritable_block_is_honestly_unauthorized() {
    use aionforge_domain::authz::DefaultAuthorizer;
    use aionforge_domain::namespace::Namespace;

    let store = store();
    let (editor_id, _) = enroll(&store, 1, AgentStatus::Active);
    let (attester_id, attester_key) = enroll(&store, 2, AgentStatus::Active);
    let principal = Principal::agent(editor_id);
    // Global ground is in every reader's visible set but never directly writable
    // under the default policy, so refusing it by name confirms nothing the read
    // surface would not already show.
    let mut b = block("global ground", BlockKind::Commitment, None);
    b.identity.namespace = Namespace::Global;
    genesis(&store, &b);
    let core = editor(&store, CoreEditPolicy::default(), false);

    let outcome = core
        .edit(
            &principal,
            &DefaultAuthorizer,
            &request(
                &b,
                "rewritten ground",
                vec![vote_for(
                    &b,
                    "rewritten ground",
                    &attester_id,
                    &attester_key,
                )],
            ),
        )
        .expect("call");
    assert_eq!(
        outcome,
        CoreEditOutcome::Unauthorized {
            namespace: Namespace::Global,
        }
    );
    let rows = store
        .audit_by_kind(AuditKind::NamespaceDenied, None, 10)
        .expect("audit")
        .events;
    assert_eq!(rows.len(), 1, "the refusal is audited");
}

#[test]
fn a_vote_never_authorizes_a_baseline_the_quorum_did_not_see() {
    let store = store();
    let (editor_id, _) = enroll(&store, 1, AgentStatus::Active);
    let (attester_id, attester_key) = enroll(&store, 2, AgentStatus::Active);
    let principal = Principal::agent(editor_id);
    let b = block("never deploy on friday", BlockKind::Commitment, None);
    genesis(&store, &b);
    let core = editor(&store, CoreEditPolicy::default(), false);
    let new_content = "never deploy on friday or saturday";

    // The attester vouched for a carry-forward edit (content only); the editor
    // attaches a baseline anchored wherever they please. The baseline slot is in the
    // signed bytes, so the smuggle is a forged voucher — this is the
    // drift-laundering primitive the attested write path forecloses.
    let carry_vote = vote_for(&b, new_content, &attester_id, &attester_key);
    let smuggled = serde_json::json!({"v": 1, "behavior_centroid": [0.0, 1.0, 0.0, 0.0]});
    let outcome = core
        .edit(
            &principal,
            &AllowAll,
            &request_with_baseline(&b, new_content, Some(smuggled.clone()), vec![carry_vote]),
        )
        .expect("call");
    assert_eq!(
        outcome,
        CoreEditOutcome::Rejected(CoreEditRejection::AttestationFailed)
    );

    // A vote over baseline X never validates a request shipping baseline Y.
    let other = serde_json::json!({"v": 1, "behavior_centroid": [1.0, 0.0, 0.0, 0.0]});
    let x_vote = vote_for_baseline(&b, new_content, Some(&other), &attester_id, &attester_key);
    let outcome = core
        .edit(
            &principal,
            &AllowAll,
            &request_with_baseline(&b, new_content, Some(smuggled.clone()), vec![x_vote]),
        )
        .expect("call");
    assert_eq!(
        outcome,
        CoreEditOutcome::Rejected(CoreEditRejection::AttestationFailed)
    );

    // The honest path: votes signed over the exact document apply, the document
    // lands, and the audit fold names the vouched baseline hash.
    let honest = vote_for_baseline(
        &b,
        new_content,
        Some(&smuggled),
        &attester_id,
        &attester_key,
    );
    let outcome = core
        .edit(
            &principal,
            &AllowAll,
            &request_with_baseline(&b, new_content, Some(smuggled.clone()), vec![honest]),
        )
        .expect("call");
    assert!(
        matches!(outcome, CoreEditOutcome::Applied(_)),
        "the quorum-vouched baseline applies: {outcome:?}"
    );
    let stored = store
        .core_block_by_id(&b.identity.id)
        .expect("read")
        .expect("present");
    assert_eq!(stored.drift_baseline, Some(smuggled.clone()));
    let applied = core_edit_rows(&store)
        .into_iter()
        .find(|row| row.payload["outcome"] == serde_json::json!("applied"))
        .expect("applied audit row");
    assert_eq!(applied.payload["rebaselined"], serde_json::json!(true));
    assert_eq!(
        applied.payload["baseline_hash"],
        serde_json::json!(
            aionforge_domain::signing::core_edit_baseline_hash(Some(&smuggled)).as_str()
        )
    );
}
