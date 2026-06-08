//! Shared scaffolding for the procedural-memory integration tests: an in-memory store, a fixed
//! clock, a controllable fake embedder, and skill/service builders. Split out so the test suites
//! stay within the file-size cap.
#![allow(dead_code)]

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::procedural::Skill;
use aionforge_domain::time::Timestamp;
use aionforge_procedural::{Clock, ProceduralConfig, ProceduralMemoryService};
use aionforge_store::{Store, StoreConfig};

/// The fixed bookkeeping instant every save/outcome is stamped with under test.
pub const NOW: &str = "2026-06-07T12:00:00-05:00[America/Chicago]";

pub fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

/// A fixed clock so stamped times are deterministic.
#[derive(Clone)]
pub struct FixedClock(pub Timestamp);

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0.clone()
    }
}

pub fn fixed_clock() -> FixedClock {
    FixedClock(ts(NOW))
}

pub fn store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store
        .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
        .expect("migrate");
    Arc::new(store)
}

pub fn stats() -> Stats {
    Stats {
        importance: 0.5,
        trust: 0.8,
        last_access: ts("2026-06-06T10:00:00-05:00[America/Chicago]"),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

pub fn identity(id: Id) -> Identity {
    Identity {
        id,
        ingested_at: ts("2026-06-06T10:00:00-05:00[America/Chicago]"),
        namespace: Namespace::Agent("alice".to_string()),
        expired_at: None,
    }
}

/// A skill with an explicit problem embedding (the service keeps a caller-supplied vector, so the
/// embedder is only exercised on the query side). `version` is a placeholder — the service
/// assigns the real one.
pub fn skill(
    name: &str,
    body: &str,
    description: &str,
    caps: &[&str],
    embedding: [f32; 4],
) -> Skill {
    let mut s = skill_no_embedding(name, body, description, caps);
    s.problem_embedding = Some(Embedding::new(embedding.to_vec()).expect("valid embedding"));
    s
}

/// A skill with no problem embedding, so the service computes it from the description on save.
pub fn skill_no_embedding(name: &str, body: &str, description: &str, caps: &[&str]) -> Skill {
    Skill {
        identity: identity(Id::generate()),
        stats: stats(),
        name: name.to_string(),
        version: 0,
        description: description.to_string(),
        problem_embedding: None,
        embedder_model: None,
        language: "python".to_string(),
        body: body.to_string(),
        params: serde_json::json!({ "type": "object" }),
        preconditions: None,
        postconditions: None,
        capabilities: caps.iter().map(|c| (*c).to_string()).collect(),
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

/// A fake embedder: a phrase → vector map with a default vector, plus an optional "down" mode for
/// the degrade tests. The mapping is computed synchronously so the returned future is `Send` and
/// borrows nothing (mirrors the capture-path fake embedder).
#[derive(Clone)]
pub struct FakeEmbedder {
    model: EmbedderModel,
    map: HashMap<String, [f32; 4]>,
    default: [f32; 4],
    down: bool,
}

impl FakeEmbedder {
    pub fn new() -> Self {
        Self {
            model: EmbedderModel {
                family: "fake".to_string(),
                version: "1".to_string(),
                dimension: 4,
            },
            map: HashMap::new(),
            default: [0.0, 0.0, 0.0, 1.0],
            down: false,
        }
    }

    /// A down embedder: every `embed` call errors.
    pub fn down() -> Self {
        Self {
            down: true,
            ..Self::new()
        }
    }

    /// Map a phrase to a fixed vector.
    #[must_use]
    pub fn with(mut self, text: &str, vector: [f32; 4]) -> Self {
        self.map.insert(text.to_string(), vector);
        self
    }

    fn lookup(&self, text: &str) -> [f32; 4] {
        self.map.get(text).copied().unwrap_or(self.default)
    }
}

impl Default for FakeEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

/// A unit error for the down embedder.
#[derive(Debug)]
pub struct FakeEmbedError;

impl std::fmt::Display for FakeEmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the fake embedder is down")
    }
}

impl std::error::Error for FakeEmbedError {}

impl Embedder for FakeEmbedder {
    type Error = FakeEmbedError;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let result = if self.down {
            Err(FakeEmbedError)
        } else {
            Ok(inputs
                .iter()
                .map(|input| {
                    Embedding::new(self.lookup(input).to_vec()).expect("valid fake embedding")
                })
                .collect())
        };
        async move { result }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

/// A service over `store` and `embedder` with default config and the fixed clock.
pub fn service(
    store: Arc<Store>,
    embedder: FakeEmbedder,
) -> ProceduralMemoryService<FakeEmbedder, FixedClock> {
    service_with(store, embedder, ProceduralConfig::default())
}

/// A service with an explicit config (for the bad-pattern penalty/threshold knob tests).
pub fn service_with(
    store: Arc<Store>,
    embedder: FakeEmbedder,
    config: ProceduralConfig,
) -> ProceduralMemoryService<FakeEmbedder, FixedClock> {
    ProceduralMemoryService::with_clock(store, embedder, Id::generate(), config, fixed_clock())
}
