//! Shared fixtures for the MCP tool-logic integration tests.
//!
//! A deterministic in-memory [`Memory`] over a fake embedder, plus the small parameter
//! builders the tool-logic tests reuse. Kept in one place so sibling test binaries
//! (`mcp.rs`, `mcp_recall_escaping.rs`, …) share one fixture rather than each carrying a copy.
//! Not every binary uses every helper, so unused items here are expected per binary.

#![allow(dead_code)]

use std::future::Future;
use std::sync::Arc;

use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::time::Timestamp;
use aionforge_engine::{Memory, MemoryConfig};
use aionforge_mcp::{CaptureToolParams, SearchToolParams};

/// A deterministic embedder: every input maps to the same unit vector, so recall ordering is
/// stable and hermetic (no network, no model).
#[derive(Clone)]
pub struct FakeEmbedder {
    model: EmbedderModel,
}

impl FakeEmbedder {
    /// A fake embedder advertising a fixed 4-dimensional model.
    #[must_use]
    pub fn new() -> Self {
        Self {
            model: EmbedderModel {
                family: "fake".to_string(),
                version: "1".to_string(),
                dimension: 4,
            },
        }
    }
}

impl Default for FakeEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

/// The error type the fake embedder never actually returns.
#[derive(Debug)]
pub struct FakeEmbedError;

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

/// A fixed wall-clock instant for deterministic capture/recall stamping.
#[must_use]
pub fn now() -> Timestamp {
    "2026-06-06T09:30:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

/// A fresh in-memory store over the fake embedder.
#[must_use]
pub fn memory() -> Arc<Memory<FakeEmbedder>> {
    Arc::new(
        Memory::open_in_memory(FakeEmbedder::new(), &now(), MemoryConfig::default())
            .expect("open memory"),
    )
}

/// A private single-capture parameter set for `agent_id`.
#[must_use]
pub fn capture_params(content: &str, agent_id: &str) -> CaptureToolParams {
    CaptureToolParams {
        content: content.to_string(),
        agent_id: Some(agent_id.to_string()),
        principal: None,
        teams: Vec::new(),
        target_namespace: None,
        role: None,
        session_id: None,
        trust: None,
        model_family: None,
        captured_at: None,
        supersedes: None,
    }
}

/// A search parameter set scoped to a single agent viewer.
#[must_use]
pub fn search_params(
    query: &str,
    agent: aionforge_domain::ids::Id,
    verbose: bool,
) -> SearchToolParams {
    SearchToolParams {
        query: query.to_string(),
        viewer: Some(format!("agent:{agent}")),
        principal: None,
        teams: Vec::new(),
        limit: None,
        verbose: Some(verbose),
        include_superseded: None,
    }
}
