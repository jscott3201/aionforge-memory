//! OpenAI-compatible embedding client.
//!
//! [`HttpEmbedder`] implements the domain [`Embedder`](aionforge_domain::contracts::Embedder)
//! contract over an OpenAI-compatible `/embeddings` endpoint. It batches in one request
//! and returns vectors in input order, fails hard when the returned count or a vector's
//! dimension is wrong, records the [`EmbedderModel`](aionforge_domain::embedding::EmbedderModel)
//! identity on the client, and L2-normalizes every vector for the cosine default. When
//! the endpoint is unreachable or returns a server error, [`EmbedError::is_unavailable`]
//! is true so a caller can degrade to lexical and graph signals (§8.1).
//!
//! Optional chat and rerank calls are not part of this milestone; the embedding path is.

mod client;
mod error;
mod wire;

pub use client::HttpEmbedder;
pub use error::EmbedError;
