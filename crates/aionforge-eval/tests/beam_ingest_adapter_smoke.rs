//! On-demand real-embedder smoke for the eval ingest adapter.
//!
//! This test stays ignored because it reads the external BEAM normalization output and
//! calls the real OpenRouter-compatible embedder. It exists to verify that the adapter
//! accepts an embed-once cache built from real BEAM text without putting any key or
//! benchmark data in CI.

// This local smoke prints skip/run guidance when invoked by hand.
#![allow(clippy::print_stdout)]

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_embed::HttpEmbedder;
use aionforge_eval::{
    IngestMode, IngestSession, parse_conversations, seed_sessions_with_embeddings,
};
use secrecy::SecretString;

const ENDPOINT: &str = "https://openrouter.ai/api/v1";
const MODEL: &str = "google/gemini-embedding-2";
const NATIVE_DIM: u32 = 3072;
const KEY_ENV: &str = "AIONFORGE_EMBEDDER_API_KEY";
const DATA_ENV: &str = "AIONFORGE_BEAM_DATA";
const LIMIT_MESSAGES: usize = 8;
const EMBED_BATCH: usize = 64;

struct CachingEmbedder {
    cache: HashMap<String, Embedding>,
    model: EmbedderModel,
}

#[derive(Debug)]
struct CacheMiss(String);

impl std::fmt::Display for CacheMiss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no cached embedding for {:?}", self.0)
    }
}

impl std::error::Error for CacheMiss {}

impl Embedder for CachingEmbedder {
    type Error = CacheMiss;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let result: Result<Vec<Embedding>, CacheMiss> = inputs
            .iter()
            .map(|input| {
                self.cache
                    .get(input)
                    .cloned()
                    .ok_or_else(|| CacheMiss(input.clone()))
            })
            .collect();
        async move { result }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

fn default_data_path() -> PathBuf {
    if let Ok(path) = std::env::var(DATA_ENV) {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join(".aionforge")
        .join("beam-data")
        .join("normalized")
        .join("beam-100k.jsonl")
}

fn model_for(dim: u32) -> EmbedderModel {
    EmbedderModel {
        family: MODEL.to_string(),
        version: String::new(),
        dimension: dim,
    }
}

async fn embed_all(embedder: &HttpEmbedder, texts: &[String]) -> HashMap<String, Embedding> {
    let mut unique = Vec::new();
    let mut seen = HashSet::new();
    for text in texts {
        if seen.insert(text.as_str()) {
            unique.push(text.clone());
        }
    }

    let mut cache = HashMap::new();
    for chunk in unique.chunks(EMBED_BATCH) {
        let vectors = embedder.embed(chunk).await.expect("embed BEAM smoke batch");
        for (text, vector) in chunk.iter().zip(vectors) {
            cache.insert(text.clone(), vector);
        }
    }
    cache
}

#[tokio::test]
#[ignore = "on-demand: reads external BEAM data and calls the real embedder"]
async fn beam_ingest_adapter_real_embedder_smoke() {
    let Ok(key) = std::env::var(KEY_ENV) else {
        println!("skipping beam_ingest_adapter_real_embedder_smoke: {KEY_ENV} unset");
        return;
    };
    let data_path = default_data_path();
    let Ok(data) = std::fs::read_to_string(&data_path) else {
        println!(
            "skipping beam_ingest_adapter_real_embedder_smoke: no BEAM data at {}",
            data_path.display()
        );
        return;
    };

    let conversations = parse_conversations(&data).expect("parse BEAM conversations");
    let conversation = conversations
        .first()
        .expect("BEAM smoke data has at least one conversation");
    let mut session = IngestSession::from(conversation);
    session.turns.truncate(LIMIT_MESSAGES);
    assert!(!session.turns.is_empty(), "BEAM smoke session has turns");

    let texts: Vec<String> = session
        .turns
        .iter()
        .map(|turn| turn.content.clone())
        .collect();
    let embedder = HttpEmbedder::new(
        ENDPOINT,
        MODEL,
        model_for(NATIVE_DIM),
        Some(SecretString::from(key)),
        Duration::from_millis(30_000),
    )
    .expect("build real embedder");
    let cache = embed_all(&embedder, &texts).await;
    let expected_turns = session.turns.len();

    let outcome = seed_sessions_with_embeddings(
        &[session],
        IngestMode::RawTurns,
        &cache,
        Some(Arc::new(CachingEmbedder {
            cache: cache.clone(),
            model: model_for(NATIVE_DIM),
        })),
        NATIVE_DIM,
    )
    .await
    .expect("seed BEAM slice through ingest adapter");

    assert_eq!(outcome.id_map.len(), expected_turns);
    println!("seeded {expected_turns} BEAM turns through the ingest adapter");
}
