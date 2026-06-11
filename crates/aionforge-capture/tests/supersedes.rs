//! Capture-path tests for the supersedes hint (04 §1 step 3).
//!
//! Hermetic, mirroring `signing_gate.rs`'s minimal-fixture shape. The hint is
//! validated at capture (live episode, writer-writable namespace), recorded in the
//! episode's origin, and echoed on the receipt; every invalid claim collapses to one
//! error so the hint is no existence oracle.

use std::future::Future;
use std::sync::Arc;

use aionforge_capture::{
    CaptureConfig, CaptureError, CaptureRequest, CaptureVerdict, Capturer, WriterContext,
};
use aionforge_domain::Capture;
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::nodes::episodic::Role;
use aionforge_domain::time::Timestamp;
use aionforge_security::CaptureFilter;
use aionforge_store::{BoundQuery, QueryResult, Store, StoreConfig, Value};

// --- A fixed hermetic embedder ------------------------------------------------------

#[derive(Clone)]
struct FixedEmbedder {
    model: EmbedderModel,
}

impl FixedEmbedder {
    fn new() -> Self {
        Self {
            model: EmbedderModel {
                family: "fixed".to_string(),
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
        f.write_str("fixed embedder error")
    }
}

impl std::error::Error for NeverError {}

impl Embedder for FixedEmbedder {
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

// --- Fixtures -------------------------------------------------------------------------

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

fn request(content: &str, agent: &Id, supersedes: Option<Id>) -> CaptureRequest {
    CaptureRequest {
        content: content.to_string(),
        role: Role::User,
        agent_id: *agent,
        teams: Vec::new(),
        session_id: None,
        captured_at: ts(),
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
        supersedes,
    }
}

fn capturer(store: Arc<Store>) -> Capturer<CaptureFilter, FixedEmbedder> {
    Capturer::new(
        store,
        CaptureFilter::with_defaults().expect("default filter"),
        FixedEmbedder::new(),
        CaptureConfig::default(),
        Arc::new(aionforge_domain::authz::DefaultAuthorizer),
    )
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

fn origin_json(store: &Store, id: &Id) -> serde_json::Value {
    match store
        .execute(
            &BoundQuery::new("MATCH (e:Episode) WHERE e.id = $id RETURN e.origin AS o")
                .bind_uuid("id", id)
                .expect("bind"),
        )
        .expect("origin query")
    {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Json(json)) => json.as_serde().clone(),
            other => panic!("expected an origin JSON, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

fn supersedes_rejected_payload(store: &Store) -> serde_json::Value {
    match store
        .execute(&BoundQuery::new(
            "MATCH (a:AuditEvent) WHERE a.kind = 'supersedes_rejected' RETURN a.payload AS p",
        ))
        .expect("payload query")
    {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Json(json)) => json.as_serde().clone(),
            other => panic!("expected one supersedes_rejected audit, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

// --- Tests ----------------------------------------------------------------------------

#[tokio::test]
async fn a_valid_hint_is_recorded_in_origin_and_echoed_on_the_receipt() {
    let store = store();
    let agent = Id::generate();
    let cap = capturer(store.clone());

    let first = cap
        .capture(request("the deploy target is staging", &agent, None))
        .await
        .expect("first capture");

    let second = cap
        .capture(request(
            "correction: the deploy target is production",
            &agent,
            Some(first.episode_id),
        ))
        .await
        .expect("hinted capture");

    // The fixed embedder makes every capture a near-duplicate of the first; what matters
    // here is that a distinct episode was written carrying the hint (near-dups still write).
    assert_ne!(second.episode_id, first.episode_id, "a new episode landed");
    assert_eq!(
        second.supersedes,
        Some(first.episode_id),
        "the receipt echoes the validated hint"
    );
    let origin = origin_json(&store, &second.episode_id);
    assert_eq!(
        origin["supersedes"],
        first.episode_id.to_string(),
        "the hint is recorded in the episode origin: {origin}"
    );
}

#[tokio::test]
async fn a_missing_target_refuses_the_capture_with_an_audit() {
    let store = store();
    let agent = Id::generate();
    let cap = capturer(store.clone());
    let ghost = Id::generate();

    let result = cap
        .capture(request("this replaces nothing real", &agent, Some(ghost)))
        .await;
    assert!(
        matches!(result, Err(CaptureError::InvalidSupersedesTarget)),
        "a ghost target refuses the capture: {result:?}"
    );
    assert_eq!(episode_count(&store), 0, "nothing is written");
    let payload = supersedes_rejected_payload(&store);
    assert_eq!(payload["reason"], "target_not_found");
    assert_eq!(payload["claimed_target"], ghost.to_string());
}

#[tokio::test]
async fn a_foreign_target_collapses_to_the_same_error_as_a_missing_one() {
    let store = store();
    let owner = Id::generate();
    let intruder = Id::generate();
    let cap = capturer(store.clone());

    let owned = cap
        .capture(request("the owner's memory", &owner, None))
        .await
        .expect("owner capture");

    // The intruder's hint names a real episode in someone else's private namespace.
    let foreign = cap
        .capture(request(
            "intruder claims a replacement",
            &intruder,
            Some(owned.episode_id),
        ))
        .await;
    let foreign_err = foreign.expect_err("a foreign target must be refused");

    // A ghost id from the same intruder must produce the IDENTICAL error: the two
    // causes are indistinguishable to the caller, so the hint is no existence oracle.
    let ghost = cap
        .capture(request(
            "intruder probes a ghost",
            &intruder,
            Some(Id::generate()),
        ))
        .await;
    let ghost_err = ghost.expect_err("a ghost target must be refused");
    assert_eq!(
        foreign_err.to_string(),
        ghost_err.to_string(),
        "foreign and missing targets are indistinguishable to the caller"
    );

    // Forensics still see the distinction, in the audit payloads only.
    let reasons = match store
        .execute(&BoundQuery::new(
            "MATCH (a:AuditEvent) WHERE a.kind = 'supersedes_rejected' RETURN a.payload AS p",
        ))
        .expect("audit query")
    {
        QueryResult::Rows(rows) => (0..rows.row_count())
            .filter_map(|i| match rows.value(i, 0) {
                Some(Value::Json(json)) => {
                    Some(json.as_serde()["reason"].as_str().unwrap_or("").to_string())
                }
                _ => None,
            })
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    assert!(
        reasons.contains(&"target_not_writable".to_string()),
        "{reasons:?}"
    );
    assert!(
        reasons.contains(&"target_not_found".to_string()),
        "{reasons:?}"
    );
}

#[tokio::test]
async fn duplicate_content_drops_the_hint_and_reports_the_dedup() {
    let store = store();
    let agent = Id::generate();
    let cap = capturer(store.clone());

    let first = cap
        .capture(request("a fact stated once", &agent, None))
        .await
        .expect("first capture");
    let target = cap
        .capture(request("an old fact to replace", &agent, None))
        .await
        .expect("target capture");

    // Identical content with a valid hint: dedup wins, nothing new is written, and the
    // receipt's verdict (not a silent success) tells the writer the hint went nowhere.
    let dup = cap
        .capture(request(
            "a fact stated once",
            &agent,
            Some(target.episode_id),
        ))
        .await
        .expect("duplicate capture");
    assert_eq!(dup.verdict, CaptureVerdict::ExactDuplicate);
    assert_eq!(dup.episode_id, first.episode_id);
    assert_eq!(dup.supersedes, None, "the hint is dropped on a duplicate");
    let origin = origin_json(&store, &first.episode_id);
    assert!(
        origin.get("supersedes").is_none(),
        "the existing episode's origin is untouched: {origin}"
    );
}
