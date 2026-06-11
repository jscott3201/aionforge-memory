//! End-to-end test for signed writes through the memory facade (06 §3, M4.T03).
//!
//! Unlike the capture-crate seam tests (which use a fake gate) and the trust-crate unit
//! tests (which use a fake resolver), this exercises the *real* composition: the engine
//! builds an Ed25519 [`SignedWriteGate`] over the store's registered agent keys, and a
//! capture only lands when the host signs the canonical provenance payload with the key the
//! store holds. The keypair is a fixed seed (no RNG). The write timestamp is the real `now`
//! so the gate's system clock sees it inside the skew window.

use std::future::Future;
use std::sync::Arc;

use aionforge_domain::blocks::Identity;
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::agent::{Agent, AgentStatus, TrustScores};
use aionforge_domain::nodes::episodic::Role;
use aionforge_domain::signing::provenance_payload;
use aionforge_domain::time::Timestamp;
use aionforge_engine::{
    CaptureError, CaptureRequest, EngineError, Memory, MemoryConfig, SecurityGate,
    SignedProvenance, WriterContext,
};
use aionforge_store::{Store, StoreConfig};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signer, SigningKey};

#[derive(Clone)]
struct FakeEmbedder {
    model: EmbedderModel,
}
impl FakeEmbedder {
    fn new() -> Self {
        Self {
            model: EmbedderModel {
                family: "fake".to_string(),
                version: "1".to_string(),
                dimension: 4,
            },
        }
    }
}
#[derive(Debug)]
struct NeverError;
impl std::fmt::Display for NeverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unreachable")
    }
}
impl std::error::Error for NeverError {}
impl Embedder for FakeEmbedder {
    type Error = NeverError;
    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let out = inputs
            .iter()
            .map(|_| Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("valid"))
            .collect();
        async move { Ok(out) }
    }
    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

fn migrate_ts() -> Timestamp {
    "2026-06-06T09:30:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn migrated_store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store.migrate(&migrate_ts()).expect("migrate");
    Arc::new(store)
}

/// Enroll an agent with `key`'s public key, returning its id.
fn enroll(store: &Store, key: &SigningKey) -> Id {
    let agent_id = Id::generate();
    let agent = Agent {
        identity: Identity {
            id: agent_id,
            ingested_at: migrate_ts(),
            namespace: Namespace::Agent("ops".to_string()),
            expired_at: None,
        },
        public_key: BASE64.encode(key.verifying_key().to_bytes()),
        model_family: "test".to_string(),
        model_version: None,
        trust_scores: TrustScores::default(),
        status: AgentStatus::Active,
    };
    store.create_agent(&agent).expect("enroll agent");
    agent_id
}

fn signed_config() -> MemoryConfig {
    MemoryConfig {
        security: SecurityGate {
            signed_writes: true,
            clock_skew_tolerance_ms: 60_000,
            ..SecurityGate::default()
        },
        ..MemoryConfig::default()
    }
}

/// A signed request: the host mints `subject`, signs `(subject, agent, captured_at)` with
/// `key`, and ships both. `captured_at` is the real `now` so the gate's clock admits it.
fn signed_request(
    content: &str,
    agent: Id,
    subject: Id,
    key: &SigningKey,
    sign_subject: Id,
) -> CaptureRequest {
    let captured_at = Timestamp::now();
    // `sign_subject` is the id actually signed over — equal to `subject` on the honest path,
    // different when a test forges a wrong-message signature.
    let payload = provenance_payload(&sign_subject, &agent, &captured_at);
    let signature = BASE64.encode(key.sign(&payload).to_bytes());
    CaptureRequest {
        content: content.to_string(),
        role: Role::User,
        agent_id: agent,
        teams: Vec::new(),
        session_id: None,
        captured_at,
        writer: WriterContext {
            model_family: "test".to_string(),
            model_version: None,
            transport: Some("library".to_string()),
            request_id: None,
            trust: 0.8,
            signed: Some(SignedProvenance {
                subject_id: subject,
                signature,
            }),
        },
        trusted: false,
        namespace: None,
        supersedes: None,
    }
}

#[tokio::test]
async fn a_correctly_signed_write_lands_with_the_host_subject_id() {
    let store = migrated_store();
    let key = SigningKey::from_bytes(&[7u8; 32]);
    let agent = enroll(&store, &key);
    let memory = Memory::new(
        Arc::clone(&store),
        FakeEmbedder::new(),
        signed_config(),
        &migrate_ts(),
    )
    .expect("build signed memory");

    let subject = Id::generate();
    let receipt = memory
        .capture(signed_request(
            "a signed turn",
            agent,
            subject,
            &key,
            subject,
        ))
        .await
        .expect("a correctly signed write is admitted");

    assert_eq!(
        receipt.episode_id, subject,
        "the host-supplied, signed subject id is adopted as the episode id"
    );
}

#[tokio::test]
async fn a_wrong_key_signature_is_rejected() {
    let store = migrated_store();
    let enrolled = SigningKey::from_bytes(&[7u8; 32]);
    let attacker = SigningKey::from_bytes(&[9u8; 32]);
    let agent = enroll(&store, &enrolled);
    let memory = Memory::new(
        Arc::clone(&store),
        FakeEmbedder::new(),
        signed_config(),
        &migrate_ts(),
    )
    .expect("build signed memory");

    let subject = Id::generate();
    // Signed by the attacker's key, which is not the one the store holds for `agent`.
    let error = memory
        .capture(signed_request("forged", agent, subject, &attacker, subject))
        .await
        .expect_err("a wrong-key signature must be rejected");

    assert!(matches!(
        error,
        EngineError::Capture(CaptureError::InvalidSignature)
    ));
}

#[tokio::test]
async fn an_unenrolled_writer_is_rejected_fail_closed() {
    let store = migrated_store();
    let key = SigningKey::from_bytes(&[7u8; 32]);
    // No enrollment: the writer's key is never registered.
    let agent = Id::generate();
    let memory = Memory::new(
        Arc::clone(&store),
        FakeEmbedder::new(),
        signed_config(),
        &migrate_ts(),
    )
    .expect("build signed memory");

    let subject = Id::generate();
    let error = memory
        .capture(signed_request("unenrolled", agent, subject, &key, subject))
        .await
        .expect_err("an unenrolled writer must be rejected");

    assert!(matches!(
        error,
        EngineError::Capture(CaptureError::InvalidSignature)
    ));
}

#[tokio::test]
async fn an_unsigned_write_is_rejected_under_a_signed_policy() {
    let store = migrated_store();
    let key = SigningKey::from_bytes(&[7u8; 32]);
    let agent = enroll(&store, &key);
    let memory = Memory::new(
        Arc::clone(&store),
        FakeEmbedder::new(),
        signed_config(),
        &migrate_ts(),
    )
    .expect("build signed memory");

    let request = CaptureRequest {
        content: "no envelope".to_string(),
        role: Role::User,
        agent_id: agent,
        teams: Vec::new(),
        session_id: None,
        captured_at: Timestamp::now(),
        writer: WriterContext {
            model_family: "test".to_string(),
            model_version: None,
            transport: None,
            request_id: None,
            trust: 0.5,
            signed: None,
        },
        trusted: false,
        namespace: None,
        supersedes: None,
    };
    let error = memory
        .capture(request)
        .await
        .expect_err("an unsigned write must be rejected when signed writes are on");

    assert!(matches!(
        error,
        EngineError::Capture(CaptureError::InvalidSignature)
    ));
}

#[tokio::test]
async fn signed_writes_off_admits_an_unsigned_write_unchanged() {
    let store = migrated_store();
    let memory = Memory::new(
        Arc::clone(&store),
        FakeEmbedder::new(),
        MemoryConfig::default(),
        &migrate_ts(),
    )
    .expect("build default memory");

    let request = CaptureRequest {
        content: "default path".to_string(),
        role: Role::User,
        agent_id: Id::generate(),
        teams: Vec::new(),
        session_id: None,
        captured_at: migrate_ts(),
        writer: WriterContext {
            model_family: "test".to_string(),
            model_version: None,
            transport: None,
            request_id: None,
            trust: 0.5,
            signed: None,
        },
        trusted: false,
        namespace: None,
        supersedes: None,
    };
    // No gate, no enrollment, no signature — the unsigned fast path commits as before.
    memory
        .capture(request)
        .await
        .expect("the default path admits an unsigned write");
}

#[test]
fn a_zero_skew_tolerance_with_signed_writes_is_a_config_error() {
    let store = migrated_store();
    let config = MemoryConfig {
        security: SecurityGate {
            signed_writes: true,
            clock_skew_tolerance_ms: 0,
            ..SecurityGate::default()
        },
        ..MemoryConfig::default()
    };
    let result = Memory::new(store, FakeEmbedder::new(), config, &migrate_ts());
    assert!(matches!(result, Err(EngineError::Config(_))));
}
