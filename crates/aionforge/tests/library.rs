//! The library acceptance smoke: a host captures and searches through the public
//! `aionforge` API alone (M1.T08).

use std::future::Future;
use std::sync::Arc;

use aionforge::Embedder;
use aionforge::{
    BlockKind, CaptureRequest, CaptureVerdict, ContentHash, CoreBlockCreate, CoreBlockDraft,
    EmbedderModel, Embedding, Id, Identity, Memory, MemoryConfig, Namespace, Principal,
    ProceduralConfig, ProceduralMemory, ProceduralMemoryService, RecallQuery, Role, Skill, Stats,
    Timestamp, WriterContext,
};

#[derive(Clone)]
struct FakeEmbedder {
    model: EmbedderModel,
}

impl FakeEmbedder {
    fn new() -> Self {
        Self::with_dimension(4)
    }

    fn with_dimension(dimension: u32) -> Self {
        Self {
            model: EmbedderModel {
                family: "fake".to_string(),
                version: "1".to_string(),
                dimension,
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

fn identity_stats(namespace: Namespace) -> (Identity, Stats) {
    (
        Identity {
            id: Id::generate(),
            ingested_at: now(),
            namespace,
            expired_at: None,
        },
        Stats {
            importance: 0.5,
            trust: 0.6,
            last_access: now(),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.0,
            is_pinned: false,
        },
    )
}

fn skill(namespace: Namespace) -> Skill {
    let body = "fn water_ferns() { schedule(\"daily\") }";
    let (identity, stats) = identity_stats(namespace);
    Skill {
        identity,
        stats,
        name: "water-ferns".to_string(),
        version: 0,
        description: "schedule fern watering reminders".to_string(),
        problem_embedding: None,
        embedder_model: None,
        language: "rust".to_string(),
        body: body.to_string(),
        params: serde_json::json!({ "type": "object" }),
        preconditions: None,
        postconditions: None,
        capabilities: vec!["calendar.write".to_string()],
        success_count: 0,
        failure_count: 0,
        mean_latency_ms: None,
        source_hash: ContentHash::of(body.as_bytes()),
        last_success_at: None,
        last_failure_at: None,
        deprecated_at: None,
        induced: false,
    }
}

#[test]
fn doctor_report_surfaces_store_and_embedder_health() {
    let memory = Memory::open_in_memory(FakeEmbedder::new(), &now(), MemoryConfig::default())
        .expect("open memory");

    let report = memory.doctor_report().expect("doctor report");

    assert!(
        report.ok,
        "fresh in-memory memory should be healthy: {report:#?}"
    );
    assert!(report.store.ok);
    assert!(report.embedder.ok);
    assert_eq!(report.embedder.model.dimension, 4);
    assert!(report.embedder.matches_store_config);
    assert!(report.embedder.vector_dimension_mismatches.is_empty());
}

#[test]
fn doctor_report_flags_live_embedder_dimension_mismatch() {
    let store = Arc::new(
        aionforge::Store::open_with_config(aionforge::StoreConfig {
            embedding_dimension: 4,
        })
        .expect("open store"),
    );
    store.migrate(&now()).expect("migrate store");
    let memory = Memory::new(
        Arc::clone(&store),
        FakeEmbedder::with_dimension(8),
        MemoryConfig::default(),
        &now(),
    )
    .expect("build memory");

    let report = memory.doctor_report().expect("doctor report");

    assert!(!report.ok, "mismatched embedder should fail health");
    assert!(report.store.ok, "store remains internally consistent");
    assert!(!report.embedder.ok);
    assert!(!report.embedder.matches_store_config);
    assert_eq!(report.embedder.store_config_dimension, 4);
    assert_eq!(report.embedder.model.dimension, 8);
    assert_eq!(report.embedder.vector_dimension_mismatches.len(), 7);
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

#[tokio::test]
async fn a_host_can_use_core_and_procedural_surfaces_through_the_library() {
    let memory = Memory::open_in_memory(FakeEmbedder::new(), &now(), MemoryConfig::default())
        .expect("open memory");

    let agent = Id::generate();
    let principal = Principal::agent(agent);
    let created = memory
        .create_core_block(
            &principal,
            CoreBlockDraft {
                namespace: principal.private(),
                content: "I keep plant care promises.".to_string(),
                block_kind: BlockKind::Commitment,
                sensitivity: None,
                importance: 0.9,
                trust: 0.9,
                embedding: None,
            },
            &now(),
        )
        .expect("create core block");
    let CoreBlockCreate::Created { block_id, .. } = created else {
        panic!("own-private core block create should be authorized");
    };
    let block = memory
        .core_block(&principal, &block_id)
        .expect("read core block")
        .expect("visible core block");
    assert_eq!(block.block_kind, BlockKind::Commitment);

    let procedural = ProceduralMemoryService::new(
        Arc::clone(memory.store()),
        FakeEmbedder::new(),
        agent,
        ProceduralConfig::default(),
    );
    let skill_id = procedural
        .save_skill(skill(principal.private()))
        .await
        .expect("save skill through public library");
    procedural
        .record_outcome(skill_id, true)
        .await
        .expect("record outcome through public library");
    let ranked = procedural
        .retrieve_skills("remember plant watering".to_string(), 3)
        .await
        .expect("retrieve skills through public library");

    assert_eq!(ranked.len(), 1);
    assert_eq!(ranked[0].skill.identity.id, skill_id);
}
