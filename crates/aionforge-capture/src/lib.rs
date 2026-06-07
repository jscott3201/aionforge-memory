//! The fast, ADD-oriented capture path (04 §1).
//!
//! [`Capturer`] is the millisecond-time write path: it filters content for privacy
//! and prompt-injection markers, deduplicates (exact content hash plus embedding
//! near-duplicate detection), embeds, attaches provenance, and commits the episode,
//! its provenance record, and a `capture` audit event through the store's single
//! mutation funnel. It is generic over the [`PrivacyFilter`](aionforge_domain::PrivacyFilter)
//! and [`Embedder`](aionforge_domain::Embedder) domain seams, so it depends on the
//! store (L0) directly but names neither the concrete security filter nor the HTTP
//! embedder.
//!
//! The path fails closed on a filter or store error and degrades on an embedder
//! error: a vector-less episode is committed for consolidation to embed later, so
//! capture never blocks on the embedder (§8.1). It never blocks on consolidation.

mod capturer;
mod config;
mod error;
mod receipt;
mod request;

pub use capturer::Capturer;
pub use config::CaptureConfig;
pub use error::CaptureError;
pub use receipt::{CaptureReceipt, CaptureVerdict, EmbeddingOutcome};
pub use request::{CaptureRequest, WriterContext};
