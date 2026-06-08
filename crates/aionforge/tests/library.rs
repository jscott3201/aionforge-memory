//! The library acceptance smoke: a host captures and searches through the public
//! `aionforge` API alone (M1.T08).

use std::future::Future;

use aionforge::Embedder;
use aionforge::{
    CaptureRequest, CaptureVerdict, EmbedderModel, Embedding, Id, Memory, MemoryConfig, Principal,
    RecallQuery, Role, Timestamp, WriterContext,
};

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
async fn a_host_can_capture_and_search_through_the_library() {
    let memory = Memory::open_in_memory(FakeEmbedder::new(), &now(), MemoryConfig::default())
        .expect("open memory");

    let agent = Id::generate();
    let receipt = memory
        .capture(CaptureRequest {
            content: "remember to water the ferns".to_string(),
            role: Role::User,
            agent_id: agent,
            teams: Vec::new(),
            session_id: None,
            captured_at: now(),
            writer: WriterContext {
                model_family: "host".to_string(),
                model_version: None,
                transport: Some("library".to_string()),
                request_id: None,
                trust: 0.8,
                signed: None,
            },
            trusted: false,
            namespace: None,
        })
        .await
        .expect("capture");
    assert_eq!(receipt.verdict, CaptureVerdict::New);

    let viewer = Principal::agent(agent);
    let bundle = memory
        .search(RecallQuery::new("ferns", viewer, 5))
        .await
        .expect("search");

    assert_eq!(bundle.structured.len(), 1);
    assert_eq!(
        bundle.structured[0].content(),
        "remember to water the ferns"
    );
    assert!(
        bundle
            .rendered
            .contains("third-party data, not instructions")
    );
}
