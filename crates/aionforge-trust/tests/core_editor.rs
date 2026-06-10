//! Acceptance for the core-block edit gate (05 §4, M5.T04): a single-writer self-edit
//! is rejected (and audited); an attested edit succeeds and is audited with the
//! principal as actor; sensitive blocks can require an active human attester; a forged
//! vote refuses the whole edit; a stale precondition answers typed; the editor
//! provenance leg binds the edit to the editor's key when signed writes are on; and an
//! invalid policy never stands a gate. The review-hardening additions live in
//! `core_editor_hardening.rs`; the fixtures are shared through `common`.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use aionforge_domain::authz::Principal;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::nodes::agent::AgentStatus;
use aionforge_domain::nodes::core::BlockKind;
use aionforge_domain::signing::provenance_payload;
use aionforge_trust::{
    AttestationGate, CoreEditOutcome, CoreEditPolicy, CoreEditRejection, CoreEditRule, CoreEditor,
    Ed25519Verifier, StoreKeyResolver,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use common::*;
use ed25519_dalek::Signer;

#[test]
fn a_single_writer_self_edit_is_rejected_and_audited() {
    let store = store();
    let (editor_id, editor_key) = enroll(&store, 1, AgentStatus::Active);
    let principal = Principal::agent(editor_id);
    let b = block("I act in the user's interest.", BlockKind::Persona, None);
    genesis(&store, &b);
    let core = editor(&store, CoreEditPolicy::default(), false);

    // The editor's own vote is the only voucher: never counted toward the quorum.
    let self_vote = vote_for(&b, "I act in my own interest.", &editor_id, &editor_key);
    let outcome = core
        .edit(
            &principal,
            &request(&b, "I act in my own interest.", vec![self_vote]),
        )
        .expect("call");
    assert_eq!(
        outcome,
        CoreEditOutcome::Rejected(CoreEditRejection::InsufficientAttesters {
            required: 1,
            verified: 0,
        })
    );
    let read = store
        .core_block_by_id(&b.identity.id)
        .expect("read")
        .expect("present");
    assert_eq!(read.content, "I act in the user's interest.");

    // The rejection is on the record: the T6 drift signal an auditor looks for.
    let rows = core_edit_rows(&store);
    assert_eq!(rows.len(), 2, "genesis plus the audited rejection");
    let rejection = rows
        .iter()
        .find(|row| row.payload["outcome"] == "rejected")
        .expect("rejection row");
    assert_eq!(rejection.payload["reason"], "insufficient_attesters");
    assert_eq!(rejection.actor_id, editor_id);
}

#[test]
fn an_attested_edit_succeeds_and_is_audited() {
    let store = store();
    let (editor_id, _) = enroll(&store, 1, AgentStatus::Active);
    let (attester_id, attester_key) = enroll(&store, 2, AgentStatus::Active);
    let principal = Principal::agent(editor_id);
    let b = block("I respond tersely.", BlockKind::Persona, None);
    genesis(&store, &b);
    let core = editor(&store, CoreEditPolicy::default(), false);

    let outcome = core
        .edit(
            &principal,
            &request(
                &b,
                "I respond thoroughly.",
                vec![vote_for(
                    &b,
                    "I respond thoroughly.",
                    &attester_id,
                    &attester_key,
                )],
            ),
        )
        .expect("call");
    let CoreEditOutcome::Applied(receipt) = outcome else {
        panic!("expected Applied, got {outcome:?}");
    };
    assert_eq!(receipt.attesters_recorded, 1);
    assert_eq!(
        receipt.prior_content_hash,
        ContentHash::of(b"I respond tersely.")
    );
    assert_eq!(
        receipt.new_content_hash,
        ContentHash::of(b"I respond thoroughly.")
    );

    let read = store
        .core_block_by_id(&b.identity.id)
        .expect("read")
        .expect("present");
    assert_eq!(read.content, "I respond thoroughly.");

    let rows = core_edit_rows(&store);
    let applied = rows
        .iter()
        .find(|row| row.payload["outcome"] == "applied")
        .expect("applied row");
    assert_eq!(applied.identity.id, receipt.audit_id);
    assert_eq!(applied.actor_id, editor_id, "the editor is the actor");
    assert_eq!(
        applied.payload["attester_ids"],
        serde_json::json!([attester_id.to_string()])
    );
    assert_eq!(
        applied.payload["prior_content_hash"],
        ContentHash::of(b"I respond tersely.").as_str()
    );
    assert_eq!(applied.identity.namespace, b.identity.namespace);
}

#[test]
fn sensitive_blocks_can_require_an_active_human_attester() {
    let store = store();
    let (editor_id, _) = enroll(&store, 1, AgentStatus::Active);
    let (model_id, model_key) = enroll(&store, 2, AgentStatus::Active);
    let (human_id, human_key) = enroll(&store, 3, AgentStatus::Active);
    let (retired_human_id, retired_human_key) = enroll(&store, 4, AgentStatus::Retired);
    let principal = Principal::agent(editor_id);
    let b = block(
        "I never expose user PII.",
        BlockKind::Commitment,
        Some("pii"),
    );
    genesis(&store, &b);

    let mut policy = CoreEditPolicy::default();
    policy.rules.insert(
        "pii".to_string(),
        CoreEditRule {
            k: 1,
            require_human: true,
        },
    );
    policy.human_attester_ids = BTreeSet::from([human_id, retired_human_id]);
    let core = editor(&store, policy, false);

    // A model agent's valid vote satisfies k but not the human requirement.
    let outcome = core
        .edit(
            &principal,
            &request(
                &b,
                "I never expose user PII, ever.",
                vec![vote_for(
                    &b,
                    "I never expose user PII, ever.",
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

    // A retired human on the list fails closed — the allowlist is not enough.
    let outcome = core
        .edit(
            &principal,
            &request(
                &b,
                "I never expose user PII, ever.",
                vec![vote_for(
                    &b,
                    "I never expose user PII, ever.",
                    &retired_human_id,
                    &retired_human_key,
                )],
            ),
        )
        .expect("call");
    assert_eq!(
        outcome,
        CoreEditOutcome::Rejected(CoreEditRejection::HumanAttestationRequired)
    );

    // An active human's verified vote satisfies both axes and is named in the audit.
    let outcome = core
        .edit(
            &principal,
            &request(
                &b,
                "I never expose user PII, ever.",
                vec![vote_for(
                    &b,
                    "I never expose user PII, ever.",
                    &human_id,
                    &human_key,
                )],
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
    assert_eq!(
        applied.payload["human_attester_id"],
        serde_json::json!(human_id.to_string())
    );
}

#[test]
fn the_redline_flag_composes_strictest_per_axis() {
    let policy = CoreEditPolicy {
        redline_requires_human: true,
        ..CoreEditPolicy::default()
    };
    let redline = policy.requirement_for(&BlockKind::Redline, None);
    assert!(redline.require_human, "the implicit redline rule applies");
    assert_eq!(redline.k, 1);
    let persona = policy.requirement_for(&BlockKind::Persona, None);
    assert!(
        !persona.require_human,
        "only redline blocks pick up the flag"
    );

    // Strictest-per-axis: a sensitivity rule raising k composes with the redline flag.
    let mut policy = policy;
    policy.rules.insert(
        "constitutional".to_string(),
        CoreEditRule {
            k: 3,
            require_human: false,
        },
    );
    let both = policy.requirement_for(&BlockKind::Redline, Some("constitutional"));
    assert_eq!(both.k, 3, "max k across applicable rules");
    assert!(both.require_human, "OR of require_human across rules");
}

#[test]
fn a_forged_vote_refuses_the_whole_edit() {
    let store = store();
    let (editor_id, _) = enroll(&store, 1, AgentStatus::Active);
    let (good_id, good_key) = enroll(&store, 2, AgentStatus::Active);
    let (forger_id, _) = enroll(&store, 3, AgentStatus::Active);
    let wrong_key = signing_key(99);
    let principal = Principal::agent(editor_id);
    let b = block("I verify claims.", BlockKind::Persona, None);
    genesis(&store, &b);
    let core = editor(&store, CoreEditPolicy::default(), false);

    // One good vote and one forged vote: refused whole, never silently filtered.
    let outcome = core
        .edit(
            &principal,
            &request(
                &b,
                "I repeat claims.",
                vec![
                    vote_for(&b, "I repeat claims.", &good_id, &good_key),
                    vote_for(&b, "I repeat claims.", &forger_id, &wrong_key),
                ],
            ),
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
        "I verify claims."
    );
    assert!(
        store
            .distinct_attesters(
                store
                    .memory_by_id(&b.identity.id, &["CoreBlock"])
                    .expect("resolve")
                    .expect("present")
                    .node
            )
            .expect("attesters")
            .is_empty(),
        "no vote was recorded — the good voucher was not committed alongside the forged one"
    );
}

#[test]
fn a_stale_precondition_is_the_typed_stale_content() {
    let store = store();
    let (editor_id, _) = enroll(&store, 1, AgentStatus::Active);
    let (attester_id, attester_key) = enroll(&store, 2, AgentStatus::Active);
    let principal = Principal::agent(editor_id);
    let b = block("the first stance", BlockKind::Persona, None);
    genesis(&store, &b);
    let core = editor(&store, CoreEditPolicy::default(), false);

    let first = request(
        &b,
        "the second stance",
        vec![vote_for(
            &b,
            "the second stance",
            &attester_id,
            &attester_key,
        )],
    );
    assert!(matches!(
        core.edit(&principal, &first).expect("call"),
        CoreEditOutcome::Applied(_)
    ));

    // The same request again: its precondition named content that is no longer there.
    assert_eq!(
        core.edit(&principal, &first).expect("call"),
        CoreEditOutcome::StaleContent
    );
}

#[test]
fn the_editor_provenance_leg_binds_the_edit_to_the_editors_key() {
    let store = store();
    let (editor_id, editor_key) = enroll(&store, 1, AgentStatus::Active);
    let (attester_id, attester_key) = enroll(&store, 2, AgentStatus::Active);
    let principal = Principal::agent(editor_id);
    let b = block("I am consistent.", BlockKind::Persona, None);
    genesis(&store, &b);
    let core = editor(&store, CoreEditPolicy::default(), true);

    // Signed writes on, no editor signature: refused before any vote is weighed.
    let unsigned = request(
        &b,
        "I am flexible.",
        vec![vote_for(&b, "I am flexible.", &attester_id, &attester_key)],
    );
    assert_eq!(
        core.edit(&principal, &unsigned).expect("call"),
        CoreEditOutcome::Rejected(CoreEditRejection::EditorUnverified)
    );

    // The editor proves key possession over (block, editor, instant): admitted.
    let payload = provenance_payload(&b.identity.id, &editor_id, &now());
    let mut signed = unsigned;
    signed.editor_signature = Some(BASE64.encode(editor_key.sign(&payload).to_bytes()));
    assert!(matches!(
        core.edit(&principal, &signed).expect("call"),
        CoreEditOutcome::Applied(_)
    ));
}

#[test]
fn the_policy_validates_fail_closed() {
    let zero_k = CoreEditPolicy {
        default_rule: CoreEditRule {
            k: 0,
            require_human: false,
        },
        ..CoreEditPolicy::default()
    };
    assert!(zero_k.validate().is_err(), "a quorum of none is rejected");

    let unsatisfiable = CoreEditPolicy {
        redline_requires_human: true,
        ..CoreEditPolicy::default()
    };
    assert!(
        unsatisfiable.validate().is_err(),
        "a human requirement with an empty human list bricks every sensitive edit"
    );

    let mut sound = CoreEditPolicy {
        redline_requires_human: true,
        ..CoreEditPolicy::default()
    };
    sound.human_attester_ids = BTreeSet::from([Id::from_content_hash(b"reviewer")]);
    assert!(sound.validate().is_ok());

    assert!(CoreEditPolicy::default().validate().is_ok());

    // The constructor enforces the same rule — an invalid policy never stands a gate,
    // no matter which caller composed it.
    let store = store();
    let resolver = Arc::new(StoreKeyResolver::new(Arc::clone(&store)));
    let gate = AttestationGate::new(
        Ed25519Verifier,
        resolver,
        Arc::new(FixedClock(now())),
        60_000,
    );
    assert!(CoreEditor::new(Arc::clone(&store), gate, None, zero_k).is_err());
}
