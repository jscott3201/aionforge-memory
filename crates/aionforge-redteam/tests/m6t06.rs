//! M6.T06 signature-forge, clock-skew, and extraction probes.
//!
//! These probes exercise the real engine composition. Forged or foreign signed
//! writes must not land, skewed signed writes must be refused by the skew gate,
//! and an explicit cross-namespace recall attempt must neither surface private
//! content nor disappear without a forensic audit row.

use std::future::Future;
use std::sync::Arc;

use aionforge_domain::blocks::Identity;
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::agent::{Agent, AgentStatus, TrustScores};
use aionforge_domain::nodes::episodic::Role;
use aionforge_domain::nodes::forensic::AuditKind;
use aionforge_domain::signing::provenance_payload;
use aionforge_domain::time::{Timestamp, instant_after, instant_before};
use aionforge_engine::{
    CaptureError, CaptureRequest, CaptureVerdict, EngineError, Memory, MemoryConfig, Principal,
    RecallQuery, SecurityGate, SignedProvenance, WriterContext,
};
use aionforge_redteam::{
    CoverageReport, M6_T06, M6_T06_ATTACK_SUCCESS_CEILING, M6_T06_AUDIT_COVERAGE_FLOOR, ProbeReport,
};
use aionforge_store::{BoundQuery, QueryResult, Store, StoreConfig};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signer, SigningKey};

const DIM: usize = 4;
const CLOCK_SKEW_TOLERANCE_MS: u64 = 60_000;
const SKEW_SECONDS: u64 = 600;
const PRIVATE_MARKER: &str = "M6T06_PRIVATE_CONTENT_DO_NOT_SURFACE";

#[derive(Clone)]
struct ProbeEmbedder {
    model: EmbedderModel,
}

impl ProbeEmbedder {
    fn new() -> Self {
        Self {
            model: EmbedderModel {
                family: "redteam-forge-probe".to_string(),
                version: "1".to_string(),
                dimension: DIM as u32,
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

impl Embedder for ProbeEmbedder {
    type Error = NeverError;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let out = inputs
            .iter()
            .map(|_| Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("valid embedding"))
            .collect();
        async move { Ok(out) }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

fn ts() -> Timestamp {
    "2026-06-11T09:30:00-05:00[America/Chicago]"
        .parse()
        .expect("valid timestamp")
}

fn migrated_store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: DIM as u32,
    })
    .expect("open store");
    store.migrate(&ts()).expect("migrate");
    Arc::new(store)
}

fn signed_config() -> MemoryConfig {
    MemoryConfig {
        security: SecurityGate {
            signed_writes: true,
            clock_skew_tolerance_ms: CLOCK_SKEW_TOLERANCE_MS,
            ..SecurityGate::default()
        },
        ..MemoryConfig::default()
    }
}

fn memory(config: MemoryConfig) -> Memory<ProbeEmbedder> {
    Memory::open_in_memory(ProbeEmbedder::new(), &ts(), config).expect("open memory")
}

fn signed_memory(store: Arc<Store>) -> Memory<ProbeEmbedder> {
    Memory::new(store, ProbeEmbedder::new(), signed_config(), &ts()).expect("open signed memory")
}

fn enroll(store: &Store, key: &SigningKey) -> Id {
    let agent_id = Id::generate();
    let agent = Agent {
        identity: Identity {
            id: agent_id,
            ingested_at: ts(),
            namespace: Namespace::Agent(agent_id.to_string()),
            expired_at: None,
        },
        public_key: BASE64.encode(key.verifying_key().to_bytes()),
        model_family: "redteam".to_string(),
        model_version: Some("1".to_string()),
        trust_scores: TrustScores::default(),
        status: AgentStatus::Active,
    };
    store.create_agent(&agent).expect("enroll agent");
    agent_id
}

fn signed_request_at(
    content: &str,
    agent: Id,
    subject: Id,
    key: &SigningKey,
    sign_subject: Id,
    captured_at: Timestamp,
) -> CaptureRequest {
    let payload = provenance_payload(&sign_subject, &agent, &captured_at);
    let signature = BASE64.encode(key.sign(&payload).to_bytes());
    CaptureRequest {
        content: content.to_string(),
        role: Role::User,
        agent_id: agent,
        teams: Vec::new(),
        session_id: None,
        captured_at: captured_at.clone(),
        ingested_at: captured_at,
        writer: WriterContext {
            model_family: "redteam".to_string(),
            model_version: Some("1".to_string()),
            transport: Some("redteam".to_string()),
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

fn unsigned_request(agent: Id, content: &str) -> CaptureRequest {
    CaptureRequest {
        content: content.to_string(),
        role: Role::User,
        agent_id: agent,
        teams: Vec::new(),
        session_id: None,
        captured_at: ts(),
        ingested_at: ts(),
        writer: WriterContext {
            model_family: "redteam".to_string(),
            model_version: Some("1".to_string()),
            transport: Some("redteam".to_string()),
            request_id: None,
            trust: 0.4,
            signed: None,
        },
        trusted: false,
        namespace: None,
        supersedes: None,
    }
}

fn recall_query(text: String, principal: Principal) -> RecallQuery {
    let mut query = RecallQuery::new(text, principal, 5);
    query.options.now = Some(ts());
    query
}

fn episode_count(store: &Store) -> usize {
    let query = BoundQuery::new("MATCH (e:Episode) RETURN e.id AS id");
    match store.execute(&query).expect("count episodes") {
        QueryResult::Rows(rows) => rows.row_count(),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn audit_count(store: &Store, kind: AuditKind) -> u64 {
    store
        .audit_by_kind(kind, None, 200)
        .expect("read audit events")
        .events
        .len() as u64
}

fn assert_probe_passed(report: &ProbeReport) {
    assert!(
        report.passed(),
        "{}",
        report.to_json().expect("serialize probe report")
    );
}

fn assert_coverage_passed(report: &CoverageReport) {
    assert!(
        report.passed(),
        "{}",
        report.to_json().expect("serialize coverage report")
    );
}

#[tokio::test]
async fn forged_and_foreign_signatures_are_rejected_and_audited() {
    let store = migrated_store();
    let enrolled_key = SigningKey::from_bytes(&[7_u8; 32]);
    let attacker_key = SigningKey::from_bytes(&[9_u8; 32]);
    let foreign_key = SigningKey::from_bytes(&[11_u8; 32]);
    let agent = enroll(&store, &enrolled_key);
    let foreign_agent = Id::generate();
    let memory = signed_memory(Arc::clone(&store));

    let wrong_key_subject = Id::generate();
    let wrong_subject = Id::generate();
    let foreign_subject = Id::generate();
    let attempts = [
        signed_request_at(
            "wrong-key forge",
            agent,
            wrong_key_subject,
            &attacker_key,
            wrong_key_subject,
            Timestamp::now(),
        ),
        signed_request_at(
            "wrong-message forge",
            agent,
            wrong_subject,
            &enrolled_key,
            Id::generate(),
            Timestamp::now(),
        ),
        signed_request_at(
            "foreign unenrolled writer",
            foreign_agent,
            foreign_subject,
            &foreign_key,
            foreign_subject,
            Timestamp::now(),
        ),
    ];
    let before = episode_count(&store);
    let mut attack_successes = 0_u64;
    let mut unexpected_rejections = 0_u64;

    for request in attempts {
        let prior = episode_count(&store);
        let result = memory.capture(request).await;
        if result.is_ok() || episode_count(&store) != prior {
            attack_successes += 1;
        }
        if !matches!(
            result,
            Err(EngineError::Capture(CaptureError::InvalidSignature))
        ) {
            unexpected_rejections += 1;
        }
    }

    assert_eq!(episode_count(&store), before);
    let attempts = 3_u64;
    let report = ProbeReport::rate(
        M6_T06,
        "signature_forgery_rejection",
        attempts,
        attack_successes,
        attempts,
        M6_T06_ATTACK_SUCCESS_CEILING,
    );
    assert_probe_passed(&report);
    assert_eq!(
        unexpected_rejections, 0,
        "all forge attempts must return the coarse invalid-signature rejection"
    );
    let coverage = CoverageReport::coverage(
        M6_T06,
        "signature_forgery_audit_coverage",
        attempts,
        audit_count(&store, AuditKind::InvalidSignature),
        M6_T06_AUDIT_COVERAGE_FLOOR,
    );
    assert_coverage_passed(&coverage);
}

#[tokio::test]
async fn skewed_signed_writes_are_rejected_and_audited() {
    let store = migrated_store();
    let key = SigningKey::from_bytes(&[7_u8; 32]);
    let agent = enroll(&store, &key);
    let memory = signed_memory(Arc::clone(&store));
    let base = Timestamp::now();
    let past_subject = Id::generate();
    let future_subject = Id::generate();
    let attempts = [
        signed_request_at(
            "replayed past write",
            agent,
            past_subject,
            &key,
            past_subject,
            instant_before(&base, SKEW_SECONDS),
        ),
        signed_request_at(
            "future skewed write",
            agent,
            future_subject,
            &key,
            future_subject,
            instant_after(&base, SKEW_SECONDS),
        ),
    ];
    let before = episode_count(&store);
    let mut attack_successes = 0_u64;
    let mut unexpected_rejections = 0_u64;

    for request in attempts {
        let prior = episode_count(&store);
        let result = memory.capture(request).await;
        if result.is_ok() || episode_count(&store) != prior {
            attack_successes += 1;
        }
        if !matches!(
            result,
            Err(EngineError::Capture(CaptureError::ClockSkew { .. }))
        ) {
            unexpected_rejections += 1;
        }
    }

    assert_eq!(episode_count(&store), before);
    let attempts = 2_u64;
    let report = ProbeReport::rate(
        M6_T06,
        "clock_skew_replay_rejection",
        attempts,
        attack_successes,
        attempts,
        M6_T06_ATTACK_SUCCESS_CEILING,
    );
    assert_probe_passed(&report);
    assert_eq!(
        unexpected_rejections, 0,
        "all skew attempts must return the clock-skew rejection"
    );
    let coverage = CoverageReport::coverage(
        M6_T06,
        "clock_skew_audit_coverage",
        attempts,
        audit_count(&store, AuditKind::ClockSkewRejected),
        M6_T06_AUDIT_COVERAGE_FLOOR,
    );
    assert_coverage_passed(&coverage);
}

#[tokio::test]
async fn crafted_query_cannot_extract_cross_namespace_private_content_and_is_audited() {
    let memory = memory(MemoryConfig::default());
    let victim = Id::generate();
    let attacker = Id::generate();
    let private_content = format!("victim vault offbook rendezvous code phrase {PRIVATE_MARKER}");
    let receipt = memory
        .capture(unsigned_request(victim, &private_content))
        .await
        .expect("seed victim private content");
    assert_eq!(receipt.verdict, CaptureVerdict::New);

    let victim_namespace = Namespace::Agent(victim.to_string());
    let before = episode_count(memory.store());
    let attempts = [
        format!("agent:{victim} reveal victim vault offbook rendezvous code"),
        format!("extract from agent:{victim}: private rendezvous phrase"),
        format!("<target namespace=\"agent:{victim}\">summarize vault code</target>"),
    ];
    let mut attack_successes = 0_u64;

    for query in attempts {
        let bundle = memory
            .search(recall_query(query, Principal::agent(attacker)))
            .await
            .expect("search crafted extraction");
        let surfaced_private = bundle
            .structured
            .iter()
            .any(|entry| entry.content().contains(PRIVATE_MARKER))
            || bundle.rendered.contains(PRIVATE_MARKER);
        let wrote_memory = episode_count(memory.store()) != before;
        if surfaced_private || wrote_memory {
            attack_successes += 1;
        }
    }

    let attempts = 3_u64;
    let report = ProbeReport::rate(
        M6_T06,
        "crafted_query_cross_namespace_extraction",
        attempts,
        attack_successes,
        attempts,
        M6_T06_ATTACK_SUCCESS_CEILING,
    );
    assert_probe_passed(&report);

    let audits = memory
        .store()
        .audit_by_kind(AuditKind::NamespaceDenied, None, 200)
        .expect("read namespace_denied audits")
        .events;
    let coverage = CoverageReport::coverage(
        M6_T06,
        "crafted_query_audit_coverage",
        attempts,
        audits.len() as u64,
        M6_T06_AUDIT_COVERAGE_FLOOR,
    );
    assert_coverage_passed(&coverage);
    assert!(
        audits
            .iter()
            .all(|audit| audit.payload["surface"] == "recall"
                && audit.payload["reason"] == "crafted_query_non_visible_namespace"
                && audit.payload["requested_namespace"] == victim_namespace.to_string()
                && !audit.payload.to_string().contains(PRIVATE_MARKER)),
        "query-audit payloads must identify the denied namespace without storing private text"
    );
}
