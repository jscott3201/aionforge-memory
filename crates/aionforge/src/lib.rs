//! The public Rust library API for the Aionforge Memory substrate.
//!
//! One type — [`Memory`] — opens the substrate, captures events, and searches them.
//! It is generic over the [`Embedder`] seam: bring the HTTP client from
//! `aionforge-embed`, or implement [`Embedder`] over any provider. Build a memory with
//! [`Memory::open_in_memory`] (or [`Memory::new`] over a store you opened), then
//! [`Memory::capture`] to write and [`Memory::search`] to read a deterministic recall
//! bundle.
//!
//! ```no_run
//! use aionforge::{Memory, MemoryConfig, CaptureRequest, RecallQuery, WriterContext};
//! use aionforge::{Embedder, Namespace, Role, Id, Timestamp};
//!
//! # async fn run<E: Embedder>(embedder: E) -> Result<(), Box<dyn std::error::Error>> {
//! let now: Timestamp = "2026-06-06T09:30:00-05:00[America/Chicago]".parse()?;
//! let memory = Memory::open_in_memory(embedder, &now, MemoryConfig::default())?;
//!
//! let agent = Id::generate();
//! memory
//!     .capture(CaptureRequest {
//!         content: "the user prefers graph databases".to_string(),
//!         role: Role::User,
//!         agent_id: agent.clone(),
//!         teams: Vec::new(),
//!         session_id: None,
//!         captured_at: now.clone(),
//!         writer: WriterContext {
//!             model_family: "host-model".to_string(),
//!             model_version: None,
//!             transport: Some("library".to_string()),
//!             request_id: None,
//!             trust: 0.9,
//!         },
//!         trusted: false,
//!         namespace: None,
//!     })
//!     .await?;
//!
//! let viewer = Namespace::Agent(agent.as_str().to_string());
//! let bundle = memory.search(RecallQuery::new("graph databases", viewer, 5)).await?;
//! println!("{}", bundle.rendered);
//! # Ok(())
//! # }
//! ```

pub use aionforge_engine::{
    CaptureConfig, CaptureReceipt, CaptureRequest, CaptureVerdict, EmbeddingOutcome, EngineError,
    EpisodeEntry, FactEntry, Memory, MemoryConfig, QueryClass, RecallBundle, RecallExplanation,
    RecallOptions, RecallQuery, RetrieverConfig, Signal, SignalWeights, Store, StoreConfig,
    StructuredEntry, TemporalMode, WriterContext,
};

pub use aionforge_domain::DomainError;
pub use aionforge_domain::contracts::{Capture, Embedder, Retriever};
pub use aionforge_domain::embedding::{EmbedderModel, Embedding};
pub use aionforge_domain::ids::{ContentHash, Id, SerializationId};
pub use aionforge_domain::namespace::Namespace;
pub use aionforge_domain::nodes::episodic::Role;
pub use aionforge_domain::time::Timestamp;
