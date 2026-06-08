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
    CaptureRequest, CaptureVerdict, Memory, MemoryConfig, RecallQuery, WriterContext,
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
            agent_id: agent.clone(),
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

    // The viewer must be the writer's private namespace to see the untrusted write.
    let viewer = Namespace::Agent(agent.as_str().to_string());
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
        agent_id: agent.clone(),
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
