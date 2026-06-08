//! Memory-kind domain types and subsystem contract traits for Aionforge Memory.
//!
//! This crate is type-only and has no I/O dependency: it names neither selene-db
//! nor any network or filesystem surface. It defines the reusable property blocks,
//! the bi-temporal validity model, the namespace and identifier types, the
//! embedding value type, the fact object-value type, and the typed error space
//! that the rest of the workspace builds on. Storage translation (domain values to
//! selene-db property maps / vectors / JSON) lives in the L0 `aionforge-store`
//! crate; this crate stays pure so its values are cheap to construct and test.
//!
//! ## Modeling conventions
//!
//! - **Serde form vs storage properties.** Field names are domain-idiomatic; the
//!   L0 `aionforge-store` crate maps them to selene-db property names where they
//!   differ — e.g. `Entity.entity_type` → the `type` property, and every
//!   `embedding` / `problem_embedding` field → the version-suffixed `embedding_v1`
//!   / `problem_embedding_v1` `VECTOR` property (02 §7). Closed string
//!   *vocabularies* (object kinds, statuses, roles, edge labels) instead serialize
//!   to their exact spec strings.
//! - **Nullable lists vs nullable JSON.** A nullable `LIST<T>` column is modeled as
//!   `Vec<T>`, with the empty vector as the canonical "absent" value (null and `[]`
//!   carry no distinct meaning for these fields); a nullable `JSON` column is an
//!   `Option`, since `None` and a JSON `null` are likewise equivalent and `None` is
//!   canonical.

pub mod authz;
pub mod blocks;
pub mod completion;
pub mod contracts;
pub mod edges;
pub mod embedding;
pub mod error;
pub mod ids;
pub mod namespace;
pub mod nodes;
pub mod signing;
pub mod time;
pub mod value;

pub use authz::{
    AuthorizationError, Authorizer, DefaultAuthorizer, DenyReason, Principal, VisibleSet,
};
pub use blocks::{Identity, Stats};
pub use completion::{ChatMessage, ChatRole, CompleterModel, Completion, CompletionRequest};
pub use contracts::{
    Capture, Completer, Consolidator, Embedder, EntitySurface, ExtractedFact, ExtractedObject,
    ExtractorIdentity, FactExtractor, FilterOutcome, Forgetting, Merge, PrivacyFilter,
    ProceduralMemory, Retriever,
};
pub use embedding::{EmbedderModel, Embedding};
pub use error::DomainError;
pub use ids::{ContentHash, Id, SerializationId};
pub use namespace::Namespace;
pub use time::{BiTemporal, Timestamp};
pub use value::{ObjectKind, ObjectValue};
