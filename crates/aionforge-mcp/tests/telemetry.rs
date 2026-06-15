//! The recall-serve telemetry emits through the `metrics` facade end to end: a real
//! `search` reaches a scoped recorder as `aionforge_mcp_recall_bytes_served_total`, with the
//! counted bytes equal to the rendered response length. With the instrumentation removed the
//! counter never fires, so this is the regression guard that keeps the response-size seam from
//! being silently dropped in a later refactor.

use std::cell::Cell;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::time::Timestamp;
use aionforge_engine::{Memory, MemoryConfig};
use aionforge_mcp::{AuthEnabled, CaptureToolParams, SearchToolParams, capture_tool, search_tool};
use metrics::{
    Counter, CounterFn, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit,
};

const SERVED_COUNTER: &str = "aionforge_mcp_recall_bytes_served_total";

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

fn memory() -> Arc<Memory<FakeEmbedder>> {
    Arc::new(
        Memory::open_in_memory(FakeEmbedder::new(), &now(), MemoryConfig::default())
            .expect("open memory"),
    )
}

fn capture_params(content: &str, agent_id: &str) -> CaptureToolParams {
    CaptureToolParams {
        content: content.to_string(),
        agent_id: Some(agent_id.to_string()),
        principal: None,
        teams: Vec::new(),
        target_namespace: None,
        role: None,
        session_id: None,
        trust: Some(0.1),
        model_family: None,
        captured_at: None,
        supersedes: None,
    }
}

fn search_params(query: &str, agent: Id) -> SearchToolParams {
    SearchToolParams {
        query: query.to_string(),
        viewer: Some(format!("agent:{agent}")),
        principal: None,
        teams: Vec::new(),
        limit: None,
        verbose: None,
        include_superseded: None,
        fanout: None,
    }
}

/// Per-name counter totals captured by [`CountingRecorder`].
type Totals = Arc<Mutex<HashMap<String, u64>>>;

/// A counter handle that folds every increment into the shared totals under its metric name
/// (labels collapse into the name bucket, which is all this test asserts on).
struct CountingCounter {
    name: String,
    totals: Totals,
}

impl CounterFn for CountingCounter {
    fn increment(&self, value: u64) {
        *self
            .totals
            .lock()
            .expect("totals lock")
            .entry(self.name.clone())
            .or_default() += value;
    }

    fn absolute(&self, value: u64) {
        let mut totals = self.totals.lock().expect("totals lock");
        let entry = totals.entry(self.name.clone()).or_default();
        *entry = (*entry).max(value);
    }
}

/// A minimal scoped recorder: counters fold into a name->total map; gauges and histograms are
/// no-ops (this test asserts only on the bytes-served counter, though `search` also emits the
/// engine's own recall histograms, which this harness safely discards).
struct CountingRecorder {
    totals: Totals,
}

impl Recorder for CountingRecorder {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        Counter::from_arc(Arc::new(CountingCounter {
            name: key.name().to_string(),
            totals: Arc::clone(&self.totals),
        }))
    }

    fn register_gauge(&self, _: &Key, _: &Metadata<'_>) -> Gauge {
        Gauge::noop()
    }

    fn register_histogram(&self, _: &Key, _: &Metadata<'_>) -> Histogram {
        Histogram::noop()
    }
}

#[test]
fn search_emits_recall_bytes_served_through_the_facade() {
    // One current-thread runtime so the scoped (thread-local) recorder stays in scope across
    // the awaited search: there is no process-global recorder to install, keeping this
    // hermetic against a parallel test binary.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let memory = memory();
    let agent = Id::generate();
    runtime
        .block_on(capture_tool(
            &memory,
            capture_params("telemetry served-bytes seed memory", &agent.to_string()),
            &now(),
            None,
            AuthEnabled(false),
        ))
        .expect("seed capture");

    let totals: Totals = Arc::new(Mutex::new(HashMap::new()));
    let recorder = CountingRecorder {
        totals: Arc::clone(&totals),
    };
    let rendered_len = Cell::new(0u64);
    metrics::with_local_recorder(&recorder, || {
        runtime.block_on(async {
            let rendered = search_tool(
                &memory,
                search_params("telemetry", agent),
                &now(),
                None,
                AuthEnabled(false),
            )
            .await
            .expect("search renders");
            // Even a zero-hit recall renders a non-empty wrapper, so served bytes are always > 0.
            assert!(!rendered.is_empty(), "search renders a non-empty wrapper");
            rendered_len.set(rendered.len() as u64);
        });
    });

    let totals = totals.lock().expect("totals lock");
    let served = totals.get(SERVED_COUNTER).copied().unwrap_or(0);
    assert!(
        served > 0,
        "{SERVED_COUNTER} must be emitted through the metrics facade; recorded: {totals:?}"
    );
    assert_eq!(
        served,
        rendered_len.get(),
        "the counter must record exactly the rendered response byte length; recorded: {totals:?}"
    );
}
