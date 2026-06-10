//! Shared fixtures for the core-block edit-gate suites (05 §4, M5.T04): a migrated
//! store, real Ed25519 enrollment, genesis blocks, a composed gate, and
//! transition-signed votes. Split out so each suite stays within the file-size cap;
//! each test binary uses a subset, so dead-code is allowed here rather than per-item.
#![allow(dead_code)]

use std::sync::Arc;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::gate::WallClock;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::agent::{Agent, AgentStatus, TrustScores};
use aionforge_domain::nodes::core::{BlockKind, CoreBlock};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::signing::core_edit_attestation_payload;
use aionforge_domain::time::Timestamp;
use aionforge_store::{Store, StoreConfig};
use aionforge_trust::{
    AttestationGate, CoreAttesterVote, CoreEditPolicy, CoreEditRequest, CoreEditor,
    Ed25519Verifier, SignedWriteGate, StoreKeyResolver,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signer, SigningKey};

pub fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

pub fn now() -> Timestamp {
    ts("2026-06-06T12:00:00-05:00[America/Chicago]")
}

pub struct FixedClock(pub Timestamp);
impl WallClock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0.clone()
    }
}

pub fn store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store
        .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
        .expect("migrate store");
    Arc::new(store)
}

pub fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

pub fn enroll(store: &Store, seed: u8, status: AgentStatus) -> (Id, SigningKey) {
    let key = signing_key(seed);
    let agent_id = Id::from_content_hash(&[seed]);
    let agent = Agent {
        identity: Identity {
            id: agent_id,
            ingested_at: now(),
            namespace: Namespace::Agent("attester".to_string()),
            expired_at: None,
        },
        public_key: BASE64.encode(key.verifying_key().to_bytes()),
        model_family: "test".to_string(),
        model_version: None,
        trust_scores: TrustScores::default(),
        status,
    };
    store.create_agent(&agent).expect("enroll agent");
    (agent_id, key)
}

pub fn block(content: &str, kind: BlockKind, sensitivity: Option<&str>) -> CoreBlock {
    CoreBlock {
        identity: Identity {
            id: Id::generate(),
            ingested_at: now(),
            namespace: Namespace::Agent("identity-owner".to_string()),
            expired_at: None,
        },
        stats: Stats {
            importance: 0.95,
            trust: 0.9,
            last_access: now(),
            access_count_recent: 1,
            referenced_count: 0,
            surprise: 0.0,
            is_pinned: false,
        },
        content: content.to_string(),
        block_kind: kind,
        sensitivity: sensitivity.map(str::to_string),
        drift_baseline: None,
        embedding: None,
        embedder_model: None,
    }
}

pub fn genesis(store: &Store, b: &CoreBlock) {
    let audit = AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(b.identity.id.to_string().as_bytes()),
            ingested_at: now(),
            namespace: b.identity.namespace.clone(),
            expired_at: None,
        },
        kind: AuditKind::CoreEdit,
        subject_id: b.identity.id,
        actor_id: Id::from_content_hash(b"creator"),
        payload: serde_json::json!({"outcome": "created"}),
        signature: String::new(),
        occurred_at: now(),
    };
    store.create_core_block(b, &audit).expect("create");
}

pub fn editor(store: &Arc<Store>, policy: CoreEditPolicy, signed_writes: bool) -> CoreEditor {
    let resolver = Arc::new(StoreKeyResolver::new(Arc::clone(store)));
    let clock = Arc::new(FixedClock(now()));
    let gate = AttestationGate::new(Ed25519Verifier, resolver.clone(), clock.clone(), 60_000);
    let editor_gate = signed_writes.then(|| {
        Arc::new(SignedWriteGate::new(
            Ed25519Verifier,
            resolver,
            clock,
            60_000,
        )) as Arc<dyn aionforge_domain::gate::ProvenanceGate>
    });
    CoreEditor::new(Arc::clone(store), gate, editor_gate, policy).expect("a validated policy")
}

/// A vote vouches for one exact transition of one block: the signed payload carries
/// the prior-content hash (from the block as the attester read it) and the hash of the
/// replacement the attester reviewed.
pub fn vote_for(
    b: &CoreBlock,
    new_content: &str,
    attester_id: &Id,
    key: &SigningKey,
) -> CoreAttesterVote {
    let payload = core_edit_attestation_payload(
        &b.identity.id,
        attester_id,
        &ContentHash::of(b.content.as_bytes()),
        &ContentHash::of(new_content.as_bytes()),
        &now(),
    );
    CoreAttesterVote {
        attester_id: *attester_id,
        attested_at: now(),
        signature_b64: BASE64.encode(key.sign(&payload).to_bytes()),
        category: None,
    }
}

pub fn request(b: &CoreBlock, new_content: &str, votes: Vec<CoreAttesterVote>) -> CoreEditRequest {
    CoreEditRequest {
        block_id: b.identity.id,
        expected_prior: ContentHash::of(b.content.as_bytes()),
        content: new_content.to_string(),
        drift_baseline: None,
        embedding: None,
        editor_signature: None,
        votes,
        at: now(),
    }
}

pub fn core_edit_rows(store: &Store) -> Vec<AuditEvent> {
    store
        .audit_by_kind(AuditKind::CoreEdit, None, 20)
        .expect("audit")
        .events
}
