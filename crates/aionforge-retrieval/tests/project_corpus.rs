//! Sanitized project-memory recall corpus.
//!
//! The fixtures are hand-curated operational memory patterns, not a raw memory export.
//! Provenance and scrub rules live in `corpus/PROVENANCE.md`.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use aionforge_domain::Retriever;
use aionforge_domain::authz::Principal;
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::time::Timestamp;
use aionforge_retrieval::{HybridRetriever, RecallOptions, RecallQuery, RetrieverConfig, Signal};
use aionforge_store::{Store, StoreConfig};
use regex::Regex;
use serde_json::Value;

const MEMORIES: &str = include_str!("corpus/project_memory.jsonl");
const QUERIES: &str = include_str!("corpus/project_queries.jsonl");
const SOURCE: &str = "sanitized-project-memory-pattern";
const DIMENSION: u32 = 8;
const T0: &str = "2026-01-01T00:00:00Z[UTC]";
const RECALL_NOW: &str = "2026-01-21T00:00:00Z[UTC]";
const MIN_EXACT_TOP_RATE: f64 = 1.0;
const MIN_MEAN_RECIPROCAL_RANK: f64 = 1.0;

#[derive(Debug, Clone)]
struct MemoryRow {
    id: String,
    text: String,
    embedding: Option<String>,
    importance: f64,
    trust: f64,
    ingested_at: Timestamp,
}

#[derive(Debug, Clone)]
struct QueryRow {
    id: String,
    query: String,
    embedding: String,
    expected_top: String,
}

#[derive(Debug, Default)]
struct CorpusMetrics {
    queries: usize,
    exact_top: usize,
    reciprocal_rank_sum: f64,
}

impl CorpusMetrics {
    fn observe(&mut self, rank: Option<usize>) {
        self.queries += 1;
        if rank == Some(0) {
            self.exact_top += 1;
        }
        if let Some(rank) = rank {
            self.reciprocal_rank_sum += 1.0 / (rank as f64 + 1.0);
        }
    }

    fn exact_top_rate(&self) -> f64 {
        self.exact_top as f64 / self.queries as f64
    }

    fn mean_reciprocal_rank(&self) -> f64 {
        self.reciprocal_rank_sum / self.queries as f64
    }
}

#[derive(Clone)]
struct FakeEmbedder {
    query_topics: HashMap<String, String>,
    model: EmbedderModel,
}

#[derive(Debug)]
struct FakeEmbedError;

impl std::fmt::Display for FakeEmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("fake corpus embedder failed")
    }
}

impl std::error::Error for FakeEmbedError {}

impl Embedder for FakeEmbedder {
    type Error = FakeEmbedError;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let embeddings = inputs
            .iter()
            .map(|input| {
                self.query_topics
                    .get(input)
                    .ok_or(FakeEmbedError)
                    .and_then(|topic| embedding_for(topic))
            })
            .collect();
        async move { embeddings }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid timestamp")
}

fn parse_memories() -> Vec<MemoryRow> {
    parse_jsonl(MEMORIES)
        .into_iter()
        .map(|v| {
            assert_source(&v);
            let embedding = string_field(&v, "embedding");
            MemoryRow {
                id: string_field(&v, "id"),
                text: string_field(&v, "text"),
                embedding: (embedding != "none").then_some(embedding),
                importance: f64_field(&v, "importance"),
                trust: f64_field(&v, "trust"),
                ingested_at: ts(&string_field(&v, "ingested_at")),
            }
        })
        .collect()
}

fn parse_queries() -> Vec<QueryRow> {
    parse_jsonl(QUERIES)
        .into_iter()
        .map(|v| {
            assert_source(&v);
            QueryRow {
                id: string_field(&v, "id"),
                query: string_field(&v, "query"),
                embedding: string_field(&v, "embedding"),
                expected_top: string_field(&v, "expected_top"),
            }
        })
        .collect()
}

fn parse_jsonl(input: &str) -> Vec<Value> {
    input
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("fixture row is valid JSON"))
        .collect()
}

fn assert_source(v: &Value) {
    assert_eq!(string_field(v, "source"), SOURCE);
}

fn string_field(v: &Value, field: &str) -> String {
    v.get(field)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("fixture row has string `{field}`"))
        .to_string()
}

fn f64_field(v: &Value, field: &str) -> f64 {
    v.get(field)
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("fixture row has numeric `{field}`"))
}

fn embedding_for(topic: &str) -> Result<Embedding, FakeEmbedError> {
    let vector = match topic {
        "ops_noise" => [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        "release" => [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        "embedder" => [0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        "doctor" => [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0],
        "auth" => [0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0],
        "plugin" => [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0],
        "backup" => [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0],
        "retrieval" => [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0],
        "container" => [0.7, 0.3, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        "public_docs" => [0.0, 0.0, 0.0, 0.0, 0.0, 0.5, 0.5, 0.0],
        _ => return Err(FakeEmbedError),
    };
    Embedding::new(vector.to_vec()).map_err(|_| FakeEmbedError)
}

fn store_with(memories: &[MemoryRow]) -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: DIMENSION,
    })
    .expect("open store");
    store.migrate(&ts(T0)).expect("migrate store");

    for row in memories {
        let episode = Episode {
            identity: identity(Id::generate(), &row.ingested_at),
            stats: stats(row.importance, row.trust, &row.ingested_at),
            content: row.text.clone(),
            role: Role::User,
            captured_at: row.ingested_at.clone(),
            agent_id: Id::generate(),
            session_id: None,
            content_hash: ContentHash::of(row.text.as_bytes()),
            embedding: row
                .embedding
                .as_ref()
                .map(|topic| embedding_for(topic).expect("fixture embedding topic is known")),
            embedder_model: None,
            consolidation_state: ConsolidationState::Raw,
            origin: None,
        };
        store.insert_episode(&episode).expect("insert fixture row");
    }

    Arc::new(store)
}

fn identity(id: Id, ingested_at: &Timestamp) -> Identity {
    Identity {
        id,
        ingested_at: ingested_at.clone(),
        namespace: Namespace::Global,
        expired_at: None,
    }
}

fn stats(importance: f64, trust: f64, last_access: &Timestamp) -> Stats {
    Stats {
        importance,
        trust,
        last_access: last_access.clone(),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

fn fake_embedder(queries: &[QueryRow]) -> FakeEmbedder {
    FakeEmbedder {
        query_topics: queries
            .iter()
            .map(|row| (row.query.clone(), row.embedding.clone()))
            .collect(),
        model: EmbedderModel {
            family: "fixture".to_string(),
            version: "project-memory-corpus".to_string(),
            dimension: DIMENSION,
        },
    }
}

async fn recall(
    r: &HybridRetriever<FakeEmbedder>,
    query: &str,
) -> aionforge_retrieval::RecallBundle {
    r.recall(RecallQuery {
        text: query.to_string(),
        principal: Principal::agent(Id::generate()),
        limit: 5,
        options: RecallOptions {
            fanout: 10,
            now: Some(ts(RECALL_NOW)),
            // Floor off: this fixed corpus pins recall/ranking precision (including
            // lexical-only and low-dense expected hits) independent of the factual class's
            // default dense floor, which is covered by min_relevance_floor.rs.
            min_relevance: Some(0.0),
            ..RecallOptions::default()
        },
    })
    .await
    .expect("recall")
}

fn assert_scrubbed(memories: &[MemoryRow], queries: &[QueryRow]) {
    let patterns = [
        ("api-key", r"sk-[A-Za-z0-9_-]{20,}"),
        ("aws-key", r"AKIA[0-9A-Z]{16}"),
        ("slack-token", r"xox[abpr]-[A-Za-z0-9-]{10,}"),
        ("github-token", r"gh[pousr]_[A-Za-z0-9]{36,}"),
        (
            "private-key",
            r"-----BEGIN (RSA|EC|OPENSSH|PGP) PRIVATE KEY-----",
        ),
        ("email", r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b"),
        (
            "uuid",
            r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
        ),
        (
            "machine-path",
            r"(/Users/|/private/var/folders|/Volumes/|~/|[A-Za-z]:\\)",
        ),
        (
            "planning-note",
            r"(?i)\b(_briefs|codex-handoffs|handoff|brief-[0-9]+|stage[ -]?[0-9]+)\b",
        ),
    ];
    let haystacks = memories
        .iter()
        .map(|row| (row.id.as_str(), row.text.as_str()))
        .chain(
            queries
                .iter()
                .map(|row| (row.id.as_str(), row.query.as_str())),
        );

    for (id, text) in haystacks {
        for (name, pattern) in patterns {
            let re = Regex::new(pattern).expect("scrub regex compiles");
            assert!(
                !re.is_match(text),
                "{id}: matched {name} scrub pattern /{pattern}/"
            );
        }
    }
}

#[tokio::test]
async fn sanitized_project_memory_corpus_recalls_expected_operational_notes() {
    let memories = parse_memories();
    let queries = parse_queries();
    assert_eq!(memories.len(), 15, "memory fixture count is intentional");
    assert_eq!(queries.len(), 13, "query fixture count is intentional");
    assert_scrubbed(&memories, &queries);

    let by_id: HashMap<&str, &MemoryRow> =
        memories.iter().map(|row| (row.id.as_str(), row)).collect();
    let id_by_text: HashMap<&str, &str> = memories
        .iter()
        .map(|row| (row.text.as_str(), row.id.as_str()))
        .collect();
    let retriever = HybridRetriever::new(
        store_with(&memories),
        fake_embedder(&queries),
        RetrieverConfig::default(),
    );

    let mut metrics = CorpusMetrics::default();
    let mut failures = Vec::new();

    for query in queries {
        let expected = by_id
            .get(query.expected_top.as_str())
            .unwrap_or_else(|| panic!("{} references a known expected row", query.id));
        let bundle = recall(&retriever, &query.query).await;
        let returned_ids: Vec<&str> = bundle
            .structured
            .iter()
            .map(|entry| {
                id_by_text
                    .get(entry.content())
                    .copied()
                    .unwrap_or("unknown")
            })
            .collect();
        let rank = returned_ids
            .iter()
            .position(|id| *id == query.expected_top.as_str());
        metrics.observe(rank);

        if rank != Some(0) {
            failures.push(format!(
                "{} expected {} first for `{}`, got {:?}",
                query.id, expected.id, query.query, returned_ids
            ));
        }

        let top = bundle
            .structured
            .first()
            .unwrap_or_else(|| panic!("{} returned at least one memory", query.id));

        if query.id == "pq-0001" && rank == Some(0) {
            let signals: Vec<Signal> = top
                .contributions()
                .iter()
                .map(|contribution| contribution.signal)
                .collect();
            assert!(
                signals.contains(&Signal::Lexical) && signals.contains(&Signal::LexicalAnchor),
                "the disk-pressure fixture pins lexical-anchor behavior"
            );
            assert!(
                bundle
                    .explanation
                    .signals_run
                    .contains(&Signal::LexicalAnchor),
                "the recall explanation reports lexical-anchor execution"
            );
        }
        if query.id == "pq-0012" && rank == Some(0) {
            let signals: Vec<Signal> = top
                .contributions()
                .iter()
                .map(|contribution| contribution.signal)
                .collect();
            assert!(
                signals.contains(&Signal::LexicalAnchor),
                "the source-path fixture pins episode lexical-anchor behavior"
            );
            assert_eq!(
                bundle.explanation.class,
                aionforge_retrieval::QueryClass::Quote,
                "source-path queries stay on the exact lexical route"
            );
        }
        if query.id == "pq-0013" && rank == Some(0) {
            let signals: Vec<Signal> = top
                .contributions()
                .iter()
                .map(|contribution| contribution.signal)
                .collect();
            assert!(
                signals.contains(&Signal::LexicalAnchor),
                "multi-anchor doc-drift queries keep lexical-anchor evidence"
            );
            assert_eq!(
                bundle.explanation.class,
                aionforge_retrieval::QueryClass::Quote,
                "multi-anchor doc-drift queries stay on the exact lexical route"
            );
        }
    }

    assert!(
        failures.is_empty(),
        "project-memory corpus regressions:\n{}",
        failures.join("\n")
    );
    assert!(
        metrics.exact_top_rate() >= MIN_EXACT_TOP_RATE,
        "exact-top rate {:.3} fell below {:.3} over {} queries",
        metrics.exact_top_rate(),
        MIN_EXACT_TOP_RATE,
        metrics.queries
    );
    assert!(
        metrics.mean_reciprocal_rank() >= MIN_MEAN_RECIPROCAL_RANK,
        "mean reciprocal rank {:.3} fell below {:.3} over {} queries",
        metrics.mean_reciprocal_rank(),
        MIN_MEAN_RECIPROCAL_RANK,
        metrics.queries
    );
}
