//! Capture-path tests for the signed-write gate (06 §3, M4.T03).
//!
//! Hermetic and crypto-free: a fake [`ProvenanceGate`] stands in for the real Ed25519 gate
//! (whose verification logic is unit-tested in `aionforge-trust`), so these tests pin the
//! *capturer's* behavior — error mapping, the audit-then-return shape, the host-supplied
//! subject id, the collision guard, and the byte-identical unsigned path — without keys.

use std::future::Future;
use std::sync::Arc;

use aionforge_capture::{
    CaptureConfig, CaptureError, CaptureRequest, Capturer, ProvenanceGate, SignedProvenance,
    WriterContext,
};
use aionforge_domain::Capture;
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::gate::{GateError, GateRejection};
use aionforge_domain::ids::Id;
use aionforge_domain::nodes::episodic::Role;
use aionforge_domain::time::Timestamp;
use aionforge_security::CaptureFilter;
use aionforge_store::{BoundQuery, QueryResult, Store, StoreConfig, Value};

// --- A minimal hermetic embedder -------------------------------------------------

#[derive(Clone)]
struct FixedEmbedder {
    model: EmbedderModel,
}

impl FixedEmbedder {
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

impl Embedder for FixedEmbedder {
    type Error = NeverError;
    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let out: Vec<Embedding> = inputs
            .iter()
            .map(|_| Embedding::new(vec![0.0, 0.0, 0.0, 1.0]).expect("valid"))
            .collect();
        async move { Ok(out) }
    }
    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

// --- A no-crypto fake gate -------------------------------------------------------

/// The outcome the fake gate yields for every `admit` call.
#[derive(Debug, Clone, Copy)]
enum Outcome {
    Admit,
    UnknownWriter,
    BadSignature,
    ClockSkew,
    Backend,
}

#[derive(Debug)]
struct FakeGate(Outcome);

impl ProvenanceGate for FakeGate {
    fn admit(
        &self,
        _subject_id: &Id,
        _writer_agent_id: &Id,
        _ingested_at: &Timestamp,
        _signature_b64: &str,
    ) -> Result<(), GateError> {
        match self.0 {
            Outcome::Admit => Ok(()),
            Outcome::UnknownWriter => Err(GateRejection::UnknownWriter.into()),
            Outcome::BadSignature => Err(GateRejection::BadSignature.into()),
            Outcome::ClockSkew => Err(GateRejection::ClockSkew {
                skew_ms: 9,
                tolerance_ms: 5,
            }
            .into()),
            Outcome::Backend => Err(GateError::Backend("backend down".to_string())),
        }
    }
}

// --- Fixtures --------------------------------------------------------------------

fn ts() -> Timestamp {
    "2026-06-06T09:30:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store.migrate(&ts()).expect("migrate store");
    Arc::new(store)
}

fn capturer_with(
    store: Arc<Store>,
    gate: Option<FakeGate>,
) -> Capturer<CaptureFilter, FixedEmbedder> {
    let base = Capturer::new(
        store,
        CaptureFilter::with_defaults().expect("default filter"),
        FixedEmbedder::new(),
        CaptureConfig::default(),
        Arc::new(aionforge_domain::authz::DefaultAuthorizer),
    );
    match gate {
        Some(gate) => base.with_gate(Arc::new(gate)),
        None => base,
    }
}

/// A request whose writer carries a signed envelope over `subject_id`.
fn signed_request(content: &str, agent: &Id, subject_id: Id) -> CaptureRequest {
    let mut request = unsigned_request(content, agent);
    request.writer.signed = Some(SignedProvenance {
        subject_id,
        signature: "ZmFrZS1zaWduYXR1cmU=".to_string(), // base64("fake-signature")
    });
    request
}

fn unsigned_request(content: &str, agent: &Id) -> CaptureRequest {
    CaptureRequest {
        content: content.to_string(),
        role: Role::User,
        agent_id: *agent,
        teams: Vec::new(),
        session_id: None,
        captured_at: ts(),
        ingested_at: ts(),
        writer: WriterContext {
            model_family: "test-writer".to_string(),
            model_version: None,
            transport: Some("library".to_string()),
            request_id: None,
            trust: 0.8,
            signed: None,
        },
        trusted: false,
        namespace: None,
        supersedes: None,
    }
}

fn episode_count(store: &Store) -> usize {
    match store
        .execute(&BoundQuery::new("MATCH (e:Episode) RETURN e.id AS id"))
        .expect("count episodes")
    {
        QueryResult::Rows(rows) => rows.row_count(),
        _ => 0,
    }
}

fn audit_count(store: &Store, kind: &str) -> usize {
    // `kind` is a trusted static literal from the test.
    let source = format!("MATCH (a:AuditEvent) WHERE a.kind = '{kind}' RETURN a.id AS id"); // gql-ident-ok
    match store
        .execute(&BoundQuery::new(source))
        .expect("count audits")
    {
        QueryResult::Rows(rows) => rows.row_count(),
        _ => 0,
    }
}

/// The decoded JSON payload of the single audit of `kind`.
fn audit_payload(store: &Store, kind: &str) -> serde_json::Value {
    let source = format!("MATCH (a:AuditEvent) WHERE a.kind = '{kind}' RETURN a.payload AS p"); // gql-ident-ok
    match store.execute(&BoundQuery::new(source)).expect("payload") {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Json(json)) => json.as_serde().clone(),
            other => panic!("expected JSON payload, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

fn provenance_signature(store: &Store, episode_id: &Id) -> Option<String> {
    match store
        .execute(
            &BoundQuery::new(
                "MATCH (e:Episode)-[:HAS_PROVENANCE]->(p:ProvenanceRecord) \
                 WHERE e.id = $id RETURN p.signature AS v",
            )
            .bind_uuid("id", episode_id)
            .expect("bind"),
        )
        .expect("query")
    {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::String(s)) => Some(s.as_str().to_string()),
            _ => None,
        },
        _ => None,
    }
}

// --- Tests: the unsigned path is untouched ---------------------------------------

#[tokio::test]
async fn no_gate_mints_server_side_and_leaves_signature_empty() {
    let store = store();
    let agent = Id::generate();
    let cap = capturer_with(store.clone(), None);

    // Even a request that carries a bogus signed envelope is ignored when no gate is set.
    let client_id = Id::generate();
    let receipt = cap
        .capture(signed_request("an unsigned-path turn", &agent, client_id))
        .await
        .expect("capture");

    assert_ne!(
        receipt.episode_id, client_id,
        "with no gate the episode id is minted server-side, not taken from the envelope"
    );
    assert_eq!(episode_count(&store), 1);
    assert_eq!(
        provenance_signature(&store, &receipt.episode_id).as_deref(),
        Some(""),
        "the unsigned path leaves the provenance signature empty"
    );
}

// --- Tests: the signed happy path ------------------------------------------------

#[tokio::test]
async fn a_signed_write_adopts_the_host_subject_id_and_records_the_signature() {
    let store = store();
    let agent = Id::generate();
    let cap = capturer_with(store.clone(), Some(FakeGate(Outcome::Admit)));

    let subject = Id::generate();
    let receipt = cap
        .capture(signed_request("a signed turn", &agent, subject))
        .await
        .expect("capture");

    assert_eq!(
        receipt.episode_id, subject,
        "the host-supplied subject id becomes the episode id"
    );
    assert_eq!(
        provenance_signature(&store, &receipt.episode_id).as_deref(),
        Some("ZmFrZS1zaWduYXR1cmU="),
        "the host signature is recorded on the provenance record"
    );
}

// --- Tests: every rejection writes an audit, no memory, and the right error -------

#[tokio::test]
async fn an_unsigned_write_under_a_signed_policy_is_rejected() {
    let store = store();
    let agent = Id::generate();
    let cap = capturer_with(store.clone(), Some(FakeGate(Outcome::Admit)));

    let error = cap
        .capture(unsigned_request("no envelope", &agent))
        .await
        .expect_err("an unsigned write must be rejected when the gate is active");

    assert!(matches!(error, CaptureError::InvalidSignature));
    assert_eq!(episode_count(&store), 0, "no memory is written");
    assert_eq!(audit_count(&store, "invalid_signature"), 1);
    assert_eq!(
        audit_payload(&store, "invalid_signature")["reason"],
        "unsigned_write_under_signed_writes"
    );
}

#[tokio::test]
async fn an_unknown_writer_is_rejected_fail_closed() {
    let store = store();
    let agent = Id::generate();
    let cap = capturer_with(store.clone(), Some(FakeGate(Outcome::UnknownWriter)));

    let error = cap
        .capture(signed_request("unknown writer", &agent, Id::generate()))
        .await
        .expect_err("unknown writer rejected");

    assert!(matches!(error, CaptureError::InvalidSignature));
    assert_eq!(episode_count(&store), 0);
    assert_eq!(audit_count(&store, "invalid_signature"), 1);
    assert_eq!(
        audit_payload(&store, "invalid_signature")["reason"],
        "unknown_writer"
    );
}

#[tokio::test]
async fn a_bad_signature_is_rejected() {
    let store = store();
    let agent = Id::generate();
    let cap = capturer_with(store.clone(), Some(FakeGate(Outcome::BadSignature)));

    let error = cap
        .capture(signed_request("bad signature", &agent, Id::generate()))
        .await
        .expect_err("bad signature rejected");

    assert!(matches!(error, CaptureError::InvalidSignature));
    assert_eq!(episode_count(&store), 0);
    assert_eq!(
        audit_payload(&store, "invalid_signature")["reason"],
        "invalid_signature"
    );
}

#[tokio::test]
async fn a_skewed_write_is_rejected_distinctly() {
    let store = store();
    let agent = Id::generate();
    let cap = capturer_with(store.clone(), Some(FakeGate(Outcome::ClockSkew)));

    let error = cap
        .capture(signed_request("skewed", &agent, Id::generate()))
        .await
        .expect_err("skew rejected");

    assert!(matches!(
        error,
        CaptureError::ClockSkew {
            skew_ms: 9,
            tolerance_ms: 5
        }
    ));
    assert_eq!(episode_count(&store), 0);
    assert_eq!(audit_count(&store, "clock_skew_rejected"), 1);
    assert_eq!(
        audit_count(&store, "invalid_signature"),
        0,
        "a skew rejection is not recorded as an invalid signature"
    );
    let payload = audit_payload(&store, "clock_skew_rejected");
    assert_eq!(payload["skew_ms"], 9);
    assert_eq!(payload["tolerance_ms"], 5);
}

#[tokio::test]
async fn a_backend_fault_is_unavailable_and_writes_no_audit() {
    let store = store();
    let agent = Id::generate();
    let cap = capturer_with(store.clone(), Some(FakeGate(Outcome::Backend)));

    let error = cap
        .capture(signed_request("backend down", &agent, Id::generate()))
        .await
        .expect_err("backend fault surfaces as an error");

    assert!(matches!(error, CaptureError::ProvenanceUnavailable(_)));
    assert_eq!(episode_count(&store), 0);
    assert_eq!(
        audit_count(&store, "invalid_signature"),
        0,
        "an availability fault is not attributed to an attacker"
    );
    assert_eq!(audit_count(&store, "clock_skew_rejected"), 0);
}

// --- Tests: the host-supplied-id collision guard ---------------------------------

#[tokio::test]
async fn a_reused_subject_id_collides_and_writes_no_second_episode() {
    let store = store();
    let agent = Id::generate();
    let cap = capturer_with(store.clone(), Some(FakeGate(Outcome::Admit)));

    let subject = Id::generate();
    cap.capture(signed_request("first content", &agent, subject))
        .await
        .expect("first signed write");
    assert_eq!(episode_count(&store), 1);

    // A second signed write reusing the same subject id over *different* content is rejected
    // by the collision guard — the content-hash dedup would not catch it (different content).
    let error = cap
        .capture(signed_request("different content", &agent, subject))
        .await
        .expect_err("a reused subject id must collide");

    assert!(matches!(error, CaptureError::InvalidSignature));
    assert_eq!(episode_count(&store), 1, "no second episode is written");
    assert_eq!(
        audit_payload(&store, "invalid_signature")["reason"],
        "subject_id_collision"
    );
}
