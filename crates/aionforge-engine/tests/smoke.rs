//! End-to-end smoke test for the memory facade: open, capture, search (M1.T08).
//!
//! Hermetic — a fake embedder stands in for the network client.

use std::future::Future;

use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::Role;
use aionforge_domain::time::Timestamp;
use aionforge_engine::{
    CaptureRequest, CaptureVerdict, Memory, MemoryConfig, Principal, RecallQuery, WriterContext,
};

/// A fake embedder that maps every input to one unit vector, so capture and search
/// always land near each other.
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
struct FakeEmbedError;

impl std::fmt::Display for FakeEmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("fake embedder is down")
    }
}

impl std::error::Error for FakeEmbedError {}

impl Embedder for FakeEmbedder {
    type Error = FakeEmbedError;

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

fn now() -> Timestamp {
    "2026-06-06T09:30:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

#[tokio::test]
async fn open_capture_then_search_round_trips() {
    let memory = Memory::new_in_memory();

    let agent = Id::generate();
    let receipt = memory
        .capture(CaptureRequest {
            content: "the user prefers graph databases".to_string(),
            role: Role::User,
            agent_id: agent,
            teams: Vec::new(),
            session_id: None,
            captured_at: now(),
            writer: WriterContext {
                model_family: "host-model".to_string(),
                model_version: Some("1".to_string()),
                transport: Some("library".to_string()),
                request_id: None,
                trust: 0.9,
            },
            trusted: false,
            namespace: None,
        })
        .await
        .expect("capture");
    assert_eq!(receipt.verdict, CaptureVerdict::New);

    // The reader is the writer, so its own private namespace is in its visible set and the
    // untrusted write surfaces.
    let viewer = Principal::agent(agent);
    let bundle = memory
        .search(RecallQuery::new("graph databases", viewer, 5))
        .await
        .expect("search");

    assert_eq!(
        bundle.structured.len(),
        1,
        "the captured memory is recalled"
    );
    assert_eq!(
        bundle.structured[0].content(),
        "the user prefers graph databases"
    );
    assert!(bundle.rendered.contains("graph databases"));
}

#[tokio::test]
async fn an_exact_duplicate_is_not_recaptured() {
    let memory = Memory::new_in_memory();
    let agent = Id::generate();
    let request = |content: &str| CaptureRequest {
        content: content.to_string(),
        role: Role::User,
        agent_id: agent,
        teams: Vec::new(),
        session_id: None,
        captured_at: now(),
        writer: WriterContext {
            model_family: "host-model".to_string(),
            model_version: None,
            transport: None,
            request_id: None,
            trust: 0.5,
        },
        trusted: false,
        namespace: None,
    };

    let first = memory
        .capture(request("same content"))
        .await
        .expect("first");
    let second = memory
        .capture(request("same content"))
        .await
        .expect("second");

    assert_eq!(first.verdict, CaptureVerdict::New);
    assert_eq!(second.verdict, CaptureVerdict::ExactDuplicate);
    assert_eq!(second.episode_id, first.episode_id);
}

#[tokio::test]
async fn a_forbidden_namespace_write_is_refused_at_the_facade() {
    let memory = Memory::new_in_memory();
    let agent = Id::generate();
    // A trusted write to global is never directly writable, so the facade refuses it (06 §1).
    let mut req = capture_request(&agent, "privileged");
    req.trusted = true;
    req.namespace = Some(Namespace::Global);

    let err = memory
        .capture(req)
        .await
        .expect_err("the facade refuses it");
    assert!(
        matches!(err, aionforge_engine::EngineError::Capture(_)),
        "a forbidden write surfaces as a capture error, got {err:?}"
    );
}

#[tokio::test]
async fn a_custom_authorizer_is_honored_by_the_facade() {
    use std::sync::Arc;

    // A deny-everything authority injected via with_authorizer refuses even an own-private write,
    // proving the seam is consulted (and is the M4.T03 signature-gating hook).
    #[derive(Debug)]
    struct DenyAll;
    impl aionforge_engine::Authorizer for DenyAll {
        fn authorize_write(
            &self,
            principal: &aionforge_engine::Principal,
            target: &Namespace,
        ) -> Result<(), aionforge_engine::AuthorizationError> {
            Err(aionforge_engine::AuthorizationError {
                agent: principal.agent_id.to_string(),
                target: target.to_string(),
                reason: aionforge_engine::DenyReason::NotDirectlyWritable,
            })
        }
        fn visible_namespaces(
            &self,
            principal: &aionforge_engine::Principal,
        ) -> aionforge_engine::VisibleSet {
            aionforge_engine::VisibleSet::new(principal.private(), Vec::new())
        }
    }

    let store = aionforge_engine::Store::open_with_config(aionforge_engine::StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store.migrate(&now()).expect("migrate");
    let memory = Memory::with_authorizer(
        Arc::new(store),
        FakeEmbedder::new(),
        MemoryConfig::default(),
        Arc::new(DenyAll),
    )
    .expect("build memory");

    let agent = Id::generate();
    let err = memory
        .capture(capture_request(&agent, "anything"))
        .await
        .expect_err("DenyAll refuses every write");
    assert!(matches!(err, aionforge_engine::EngineError::Capture(_)));
}

/// A capture request for `agent` with the given content — untrusted, private, no teams.
fn capture_request(agent: &Id, content: &str) -> CaptureRequest {
    CaptureRequest {
        content: content.to_string(),
        role: Role::User,
        agent_id: *agent,
        teams: Vec::new(),
        session_id: None,
        captured_at: now(),
        writer: WriterContext {
            model_family: "host-model".to_string(),
            model_version: None,
            transport: None,
            request_id: None,
            trust: 0.5,
        },
        trusted: false,
        namespace: None,
    }
}

/// A small helper so each test reads cleanly.
trait OpenInMemory {
    fn new_in_memory() -> Memory<FakeEmbedder>;
}

impl OpenInMemory for Memory<FakeEmbedder> {
    fn new_in_memory() -> Memory<FakeEmbedder> {
        Memory::open_in_memory(FakeEmbedder::new(), &now(), MemoryConfig::default())
            .expect("open in-memory memory")
    }
}
