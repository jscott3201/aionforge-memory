//! M5.T04 acceptance for the core-block facade (05 §4), end to end through `Memory`:
//!
//! - The gate is **always on**: an unconfigured memory stands it up, and the
//!   all-default posture is the spec's floor (one non-editor attester), not an off
//!   state. An invalid strictness policy — or a zero clock-skew window, which the
//!   always-on gate consumes — is refused at construction, before any subsystem runs.
//! - A genesis create is namespace-authorized: an owner creates in its own ground and
//!   gets the block plus its `core_edit` genesis audit in one commit; a write outside
//!   the principal's authority is the typed refusal with a `namespace_denied` audit.
//! - A full attested edit flows through the facade with real Ed25519
//!   transition-signed votes; a single-writer self-edit is rejected and audited; an
//!   outsider with valid votes is refused on authority — attesters vouch for content,
//!   never for authority.
//! - Reads are scoped by the principal's visible set.

mod common;

use std::sync::Arc;

use aionforge_domain::authz::Principal;
use aionforge_domain::blocks::Identity;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::agent::{Agent, AgentStatus, TrustScores};
use aionforge_domain::nodes::core::BlockKind;
use aionforge_domain::nodes::forensic::AuditKind;
use aionforge_domain::signing::{core_edit_attestation_payload, core_edit_baseline_hash};
use aionforge_domain::time::Timestamp;
use aionforge_engine::{
    CoreAttesterVote, CoreBlockCreate, CoreBlockDraft, CoreEditOutcome, CoreEditPolicy,
    CoreEditRejection, CoreEditRequest, CoreEditRule, EngineError, Memory, MemoryConfig,
};
use aionforge_store::Store;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use common::{FakeEmbedder, migrated_store, ts};
use ed25519_dalek::{Signer, SigningKey};

fn memory(store: &Arc<Store>) -> Memory<FakeEmbedder> {
    Memory::new(
        Arc::clone(store),
        FakeEmbedder::new(),
        MemoryConfig::default(),
        &ts(0),
    )
    .expect("memory")
}

fn enroll(store: &Store, seed: u8) -> (Id, SigningKey) {
    let key = SigningKey::from_bytes(&[seed; 32]);
    let agent_id = Id::from_content_hash(&[seed]);
    let agent = Agent {
        identity: Identity {
            id: agent_id,
            ingested_at: ts(0),
            namespace: Namespace::Agent(agent_id.to_string()),
            expired_at: None,
        },
        public_key: BASE64.encode(key.verifying_key().to_bytes()),
        model_family: "test".to_string(),
        model_version: None,
        trust_scores: TrustScores::default(),
        status: AgentStatus::Active,
    };
    store.create_agent(&agent).expect("enroll agent");
    (agent_id, key)
}

fn draft(namespace: Namespace, content: &str) -> CoreBlockDraft {
    CoreBlockDraft {
        namespace,
        content: content.to_string(),
        block_kind: BlockKind::Persona,
        sensitivity: None,
        importance: 0.95,
        trust: 0.9,
        embedding: None,
    }
}

/// A transition-signed vote stamped against the live system clock — the engine's
/// always-on attestation gate runs `SystemWallClock`, so the instant must sit inside
/// the real skew window.
fn vote(
    block_id: &Id,
    prior: &str,
    new: &str,
    attester_id: &Id,
    key: &SigningKey,
) -> (CoreAttesterVote, Timestamp) {
    let at = Timestamp::now();
    let payload = core_edit_attestation_payload(
        block_id,
        attester_id,
        &ContentHash::of(prior.as_bytes()),
        &ContentHash::of(new.as_bytes()),
        &core_edit_baseline_hash(None),
        &at,
    );
    (
        CoreAttesterVote {
            attester_id: *attester_id,
            attested_at: at.clone(),
            signature_b64: BASE64.encode(key.sign(&payload).to_bytes()),
            category: None,
        },
        at,
    )
}

fn edit_request(
    block_id: Id,
    prior: &str,
    new: &str,
    votes: Vec<CoreAttesterVote>,
    at: Timestamp,
) -> CoreEditRequest {
    CoreEditRequest {
        block_id,
        expected_prior: ContentHash::of(prior.as_bytes()),
        content: new.to_string(),
        drift_baseline: None,
        embedding: None,
        editor_signature: None,
        votes,
        at,
    }
}

#[test]
fn an_owner_creates_in_its_own_ground_with_a_genesis_audit() {
    let store = migrated_store();
    let memory = memory(&store);
    let (owner_id, _) = enroll(&store, 1);
    let owner = Principal::agent(owner_id);

    let outcome = memory
        .create_core_block(
            &owner,
            draft(owner.private(), "I am a careful reviewer."),
            &ts(1),
        )
        .expect("create");
    let CoreBlockCreate::Created { block_id, audit_id } = outcome else {
        panic!("expected Created, got {outcome:?}");
    };

    let read = memory
        .core_block(&owner, &block_id)
        .expect("read")
        .expect("visible to its owner");
    assert_eq!(read.content, "I am a careful reviewer.");
    assert_eq!(read.identity.namespace, owner.private());
    assert!(
        read.drift_baseline.is_none(),
        "the baseline is the drift detector's call, never the writer's"
    );

    let rows = store
        .audit_by_kind(AuditKind::CoreEdit, None, 10)
        .expect("audit")
        .events;
    assert_eq!(rows.len(), 1, "one genesis, one row");
    assert_eq!(rows[0].identity.id, audit_id);
    assert_eq!(rows[0].subject_id, block_id);
    assert_eq!(rows[0].actor_id, owner_id, "the creator is the actor");
    assert_eq!(rows[0].payload["outcome"], "created");
    assert_eq!(
        rows[0].payload["new_content_hash"],
        ContentHash::of(b"I am a careful reviewer.").as_str()
    );
}

#[test]
fn a_create_outside_write_authority_is_refused_and_audited() {
    let store = migrated_store();
    let memory = memory(&store);
    let (agent_id, _) = enroll(&store, 1);
    let principal = Principal::agent(agent_id);

    // Global is never directly writable; another agent's private ground is not yours.
    for target in [
        Namespace::Global,
        Namespace::Agent("someone-else".to_string()),
    ] {
        let outcome = memory
            .create_core_block(
                &principal,
                draft(target.clone(), "smuggled identity"),
                &ts(1),
            )
            .expect("call");
        assert_eq!(
            outcome,
            CoreBlockCreate::Unauthorized {
                namespace: target.clone()
            },
            "{target:?} is refused"
        );
    }
    assert!(
        store.live_core_blocks().expect("scan").is_empty(),
        "nothing was written"
    );

    let denials = store
        .audit_by_kind(AuditKind::NamespaceDenied, None, 10)
        .expect("audit")
        .events;
    assert_eq!(denials.len(), 2, "each refusal is on the record");
    for row in &denials {
        assert_eq!(row.actor_id, agent_id);
        assert_eq!(row.payload["surface"], "core_block_create");
        assert_eq!(row.identity.namespace, Namespace::System);
    }
}

#[test]
fn the_attested_edit_flows_end_to_end_through_the_facade() {
    let store = migrated_store();
    let memory = memory(&store);
    let (editor_id, _) = enroll(&store, 1);
    let (attester_id, attester_key) = enroll(&store, 2);
    let editor = Principal::agent(editor_id);
    let prior = "I respond tersely.";
    let revised = "I respond thoroughly, with sources.";

    let CoreBlockCreate::Created { block_id, .. } = memory
        .create_core_block(&editor, draft(editor.private(), prior), &ts(1))
        .expect("create")
    else {
        panic!("create");
    };

    // A single-writer self-edit is rejected and audited (the T6 drift signal).
    let no_votes = edit_request(block_id, prior, revised, vec![], Timestamp::now());
    let outcome = memory.edit_core_block(&editor, &no_votes).expect("call");
    assert_eq!(
        outcome,
        CoreEditOutcome::Rejected(CoreEditRejection::InsufficientAttesters {
            required: 1,
            verified: 0,
        })
    );

    // With a transition-signed second attester, the edit applies in place.
    let (vote, at) = vote(&block_id, prior, revised, &attester_id, &attester_key);
    let outcome = memory
        .edit_core_block(
            &editor,
            &edit_request(block_id, prior, revised, vec![vote], at),
        )
        .expect("call");
    let CoreEditOutcome::Applied(receipt) = outcome else {
        panic!("expected Applied, got {outcome:?}");
    };
    assert_eq!(receipt.attesters_recorded, 1);

    let read = memory
        .core_block(&editor, &block_id)
        .expect("read")
        .expect("present");
    assert_eq!(read.content, revised, "same id, new content, one node");

    let rows = store
        .audit_by_kind(AuditKind::CoreEdit, None, 10)
        .expect("audit")
        .events;
    let applied = rows
        .iter()
        .find(|row| row.payload["outcome"] == "applied")
        .expect("applied row");
    assert_eq!(applied.identity.id, receipt.audit_id);
    let rejected = rows
        .iter()
        .find(|row| row.payload["outcome"] == "rejected")
        .expect("rejection row");
    assert_eq!(rejected.payload["reason"], "insufficient_attesters");
}

#[test]
fn an_outsider_cannot_edit_someone_elses_identity_with_any_votes() {
    let store = migrated_store();
    let memory = memory(&store);
    let (owner_id, _) = enroll(&store, 1);
    let (outsider_id, _) = enroll(&store, 2);
    let (attester_id, attester_key) = enroll(&store, 3);
    let owner = Principal::agent(owner_id);
    let outsider = Principal::agent(outsider_id);
    let prior = "the owner's stance";

    let CoreBlockCreate::Created { block_id, .. } = memory
        .create_core_block(&owner, draft(owner.private(), prior), &ts(1))
        .expect("create")
    else {
        panic!("create");
    };

    let (vote, at) = vote(&block_id, prior, "rewritten", &attester_id, &attester_key);
    let outcome = memory
        .edit_core_block(
            &outsider,
            &edit_request(block_id, prior, "rewritten", vec![vote], at),
        )
        .expect("call");
    // The owner's private block is outside the outsider's visible set: the refusal
    // answers exactly like an absent id, mirroring the read path's no-oracle rule.
    assert_eq!(
        outcome,
        CoreEditOutcome::NotFound,
        "attesters vouch for content, never for authority — and an invisible \
         block stays invisible"
    );
    assert_eq!(
        memory
            .core_block(&owner, &block_id)
            .expect("read")
            .expect("present")
            .content,
        prior
    );
}

#[test]
fn an_invalid_core_block_posture_is_refused_at_construction() {
    let store = migrated_store();

    // A zero k anywhere re-enables single-writer edits: refused.
    let zero_k = MemoryConfig {
        core_block: CoreEditPolicy {
            default_rule: CoreEditRule {
                k: 0,
                require_human: false,
            },
            ..CoreEditPolicy::default()
        },
        ..MemoryConfig::default()
    };
    assert!(matches!(
        Memory::new(Arc::clone(&store), FakeEmbedder::new(), zero_k, &ts(0)),
        Err(EngineError::Config(_))
    ));

    // A human requirement with an empty allowlist bricks every sensitive edit: refused.
    let unsatisfiable = MemoryConfig {
        core_block: CoreEditPolicy {
            redline_requires_human: true,
            ..CoreEditPolicy::default()
        },
        ..MemoryConfig::default()
    };
    assert!(matches!(
        Memory::new(
            Arc::clone(&store),
            FakeEmbedder::new(),
            unsatisfiable,
            &ts(0)
        ),
        Err(EngineError::Config(_))
    ));

    // The always-on gate consumes the skew window even with signed writes off, so a
    // zero window is a construction error, not a memory that silently refuses every
    // identity edit.
    let mut zero_skew = MemoryConfig::default();
    zero_skew.security.signed_writes = false;
    zero_skew.security.clock_skew_tolerance_ms = 0;
    assert!(matches!(
        Memory::new(Arc::clone(&store), FakeEmbedder::new(), zero_skew, &ts(0)),
        Err(EngineError::Config(_))
    ));
}

#[test]
fn reads_are_scoped_to_the_principals_visible_set() {
    let store = migrated_store();
    let memory = memory(&store);
    let (member_id, _) = enroll(&store, 1);
    let (outsider_id, _) = enroll(&store, 2);
    let member = Principal::new(member_id, vec!["squad".to_string()]);
    let outsider = Principal::agent(outsider_id);

    let CoreBlockCreate::Created { block_id, .. } = memory
        .create_core_block(
            &member,
            draft(
                Namespace::Team("squad".to_string()),
                "the squad's shared commitments",
            ),
            &ts(1),
        )
        .expect("create")
    else {
        panic!("create");
    };

    assert!(
        memory
            .core_block(&member, &block_id)
            .expect("read")
            .is_some(),
        "a team member sees the team block"
    );
    assert_eq!(memory.live_core_blocks(&member).expect("scan").len(), 1);

    assert!(
        memory
            .core_block(&outsider, &block_id)
            .expect("read")
            .is_none(),
        "a non-member cannot tell the block exists"
    );
    assert!(memory.live_core_blocks(&outsider).expect("scan").is_empty());
}

#[tokio::test]
async fn the_editor_provenance_leg_runs_end_to_end_with_signed_writes_on() {
    use aionforge_domain::signing::provenance_payload;
    use aionforge_engine::SecurityGate;

    let store = migrated_store();
    let config = MemoryConfig {
        security: SecurityGate {
            signed_writes: true,
            ..SecurityGate::default()
        },
        ..MemoryConfig::default()
    };
    let memory = Memory::new(Arc::clone(&store), FakeEmbedder::new(), config, &ts(0))
        .expect("memory with signed writes");
    let (editor_id, editor_key) = enroll(&store, 1);
    let (attester_id, attester_key) = enroll(&store, 2);
    let editor = Principal::agent(editor_id);
    let prior = "I hold the line.";
    let revised = "I hold the line, and I say when I cannot.";

    let CoreBlockCreate::Created { block_id, .. } = memory
        .create_core_block(&editor, draft(editor.private(), prior), &ts(1))
        .expect("create")
    else {
        panic!("create");
    };

    // No editor signature: refused before any vote is weighed.
    let (attester_vote, at) = vote(&block_id, prior, revised, &attester_id, &attester_key);
    let unsigned = edit_request(
        block_id,
        prior,
        revised,
        vec![attester_vote.clone()],
        at.clone(),
    );
    assert_eq!(
        memory.edit_core_block(&editor, &unsigned).expect("call"),
        CoreEditOutcome::Rejected(CoreEditRejection::EditorUnverified)
    );

    // The editor proves key possession over (block, editor, instant): applied.
    let mut signed = unsigned;
    let payload = provenance_payload(&block_id, &editor_id, &at);
    signed.editor_signature = Some(BASE64.encode(editor_key.sign(&payload).to_bytes()));
    let outcome = memory.edit_core_block(&editor, &signed).expect("call");
    assert!(
        matches!(outcome, CoreEditOutcome::Applied(_)),
        "{outcome:?}"
    );
    assert_eq!(
        memory
            .core_block(&editor, &block_id)
            .expect("read")
            .expect("present")
            .content,
        revised
    );
}

#[tokio::test]
async fn a_create_carries_its_embedding_pair() {
    use aionforge_domain::embedding::{EmbedderModel, Embedding};
    use common::DIM;

    let store = migrated_store();
    let memory = memory(&store);
    let (owner_id, _) = enroll(&store, 1);
    let owner = Principal::agent(owner_id);

    let mut with_vector = draft(owner.private(), "an embedded persona");
    with_vector.embedding = Some((
        Embedding::new(vec![0.5; DIM as usize]).expect("embedding"),
        EmbedderModel {
            family: "fake".to_string(),
            version: "1".to_string(),
            dimension: DIM,
        },
    ));
    let CoreBlockCreate::Created { block_id, .. } = memory
        .create_core_block(&owner, with_vector, &ts(1))
        .expect("create")
    else {
        panic!("create");
    };

    let read = memory
        .core_block(&owner, &block_id)
        .expect("read")
        .expect("present");
    assert!(read.embedding.is_some(), "the vector landed");
    assert_eq!(read.embedder_model.expect("model").dimension, DIM);
}
