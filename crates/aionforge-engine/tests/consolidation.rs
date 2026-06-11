//! Facade smoke test for background consolidation (M2.T04): capturing an episode and
//! then starting the consolidator turns it into a derived fact, exercising
//! `Memory::start_consolidation` and `Memory::embedder` end to end over the real stack.
//!
//! Hermetic — a fake embedder stands in for the network client. The episode has a single
//! subject and object, so resolution is unambiguous regardless of the fake's vectors.

use std::future::Future;
use std::time::Duration;

use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::nodes::episodic::Role;
use aionforge_domain::time::Timestamp;
use aionforge_engine::{
    CaptureRequest, CaptureVerdict, ConsolidationConfig, EngineError, Memory, MemoryConfig,
    PassConfig, RuleExtractor, RuleInducer, RuleSummarizer, WriterContext,
};
use aionforge_store::{BoundQuery, QueryResult};

/// A fake embedder mapping every input to one unit vector (dimension 4).
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
struct NeverFails;

impl std::fmt::Display for NeverFails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unreachable")
    }
}

impl std::error::Error for NeverFails {}

impl Embedder for FakeEmbedder {
    type Error = NeverFails;

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

fn fact_count(memory: &Memory<FakeEmbedder>) -> usize {
    let query = BoundQuery::new("MATCH (f:Fact) RETURN f.id AS id");
    match memory.store().execute(&query).expect("fact count query") {
        QueryResult::Rows(rows) => rows.row_count(),
        _ => 0,
    }
}

#[tokio::test]
async fn capture_then_start_consolidation_derives_a_fact() {
    let memory =
        Memory::open_in_memory(FakeEmbedder::new(), &now(), MemoryConfig::default()).expect("open");

    // The embedder accessor exposes the shared model the facade was built with.
    assert_eq!(memory.embedder().model().dimension, 4);

    let agent = Id::generate();
    let receipt = memory
        .capture(CaptureRequest {
            content: "Alice works on Aionforge".to_string(),
            role: Role::User,
            agent_id: agent,
            teams: Vec::new(),
            session_id: None,
            captured_at: now(),
            writer: WriterContext {
                model_family: "host".to_string(),
                model_version: None,
                transport: None,
                request_id: None,
                trust: 0.9,
                signed: None,
            },
            trusted: false,
            namespace: None,
        })
        .await
        .expect("capture");
    assert_eq!(receipt.verdict, CaptureVerdict::New);
    assert_eq!(
        fact_count(&memory),
        0,
        "no fact exists until consolidation runs"
    );

    let handle = memory.start_consolidation(
        RuleExtractor::with_default_rules(),
        RuleSummarizer::with_default_rules(),
        RuleInducer::with_default_rules(),
        ConsolidationConfig {
            tick_interval: Duration::from_millis(25),
            ..ConsolidationConfig::default()
        },
        PassConfig::default(),
    );

    // Poll until the background loop derives the fact, bounded so a regression fails fast.
    // Sleep before the first check so the spawned consolidator has yielded and ticked at
    // least once — the count is never read before the loop can possibly have run.
    let mut derived = false;
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        if fact_count(&memory) >= 1 {
            derived = true;
            break;
        }
    }
    handle.shutdown().await;

    assert!(
        derived,
        "the background consolidator derived a fact from the captured episode"
    );
}

#[tokio::test]
async fn capture_then_consolidate_once_derives_a_fact() {
    let memory =
        Memory::open_in_memory(FakeEmbedder::new(), &now(), MemoryConfig::default()).expect("open");
    let receipt = memory
        .capture(CaptureRequest {
            content: "Alice works on Aionforge".to_string(),
            role: Role::User,
            agent_id: Id::generate(),
            teams: Vec::new(),
            session_id: None,
            captured_at: now(),
            writer: WriterContext {
                model_family: "host".to_string(),
                model_version: None,
                transport: None,
                request_id: None,
                trust: 0.9,
                signed: None,
            },
            trusted: false,
            namespace: None,
        })
        .await
        .expect("capture");
    assert_eq!(receipt.verdict, CaptureVerdict::New);
    assert_eq!(
        fact_count(&memory),
        0,
        "no fact exists until consolidation runs"
    );

    let report = memory
        .consolidate_once(
            RuleExtractor::with_default_rules(),
            RuleSummarizer::with_default_rules(),
            RuleInducer::with_default_rules(),
            ConsolidationConfig::default(),
            PassConfig::default(),
        )
        .await
        .expect("foreground consolidation");

    assert_eq!(report.consolidated, 1);
    assert_eq!(report.retried, 0);
    assert_eq!(report.failed, 0);
    assert_eq!(report.pending_after, 0);
    assert!(
        fact_count(&memory) >= 1,
        "foreground consolidation derived a fact"
    );
}

/// One minute after [`now`], for measuring lag against a fresh capture.
fn a_minute_later() -> Timestamp {
    "2026-06-06T09:31:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

#[tokio::test]
async fn consolidation_lag_reports_a_pending_capture_without_reaching_into_the_store() {
    let memory =
        Memory::open_in_memory(FakeEmbedder::new(), &now(), MemoryConfig::default()).expect("open");
    memory
        .capture(CaptureRequest {
            content: "Alice works on Aionforge".to_string(),
            role: Role::User,
            agent_id: Id::generate(),
            teams: Vec::new(),
            session_id: None,
            captured_at: now(),
            writer: WriterContext {
                model_family: "host".to_string(),
                model_version: None,
                transport: None,
                request_id: None,
                trust: 0.9,
                signed: None,
            },
            trusted: false,
            namespace: None,
        })
        .await
        .expect("capture");

    // The facade resolves the backlog against an injected clock — no L0 access from the host.
    let lag = memory
        .consolidation_lag(&a_minute_later())
        .expect("lag query");
    assert_eq!(lag.episodes_pending, 1, "the raw capture is pending");
    assert_eq!(lag.episodes_failed, 0);
    assert!(
        lag.oldest_pending_lag >= Duration::from_secs(1),
        "a minute elapsed since capture: {:?}",
        lag.oldest_pending_lag
    );
}

#[test]
fn new_rejects_an_out_of_range_capture_config() {
    let mut config = MemoryConfig::default();
    config.capture.near_duplicate_threshold = 2.0; // outside [0, 1]
    // `Memory` is not `Debug`, so match the result rather than unwrapping the Ok side.
    let result = Memory::open_in_memory(FakeEmbedder::new(), &now(), config);
    assert!(
        matches!(result, Err(EngineError::Config(_))),
        "an out-of-range threshold is rejected with a config error"
    );
}
