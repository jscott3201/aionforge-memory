//! L0 storage over the selene-db `SharedGraph`.
//!
//! This is the only crate that names selene-db. It owns the graph, exposes typed
//! read/write that commits through the engine's single committer thread, takes
//! lock-free snapshot reads, runs caller-influenced queries through bound
//! parameters only, and translates domain values to and from selene-db property
//! maps, vectors, and JSON.
//! Higher layers depend on this crate, not on selene-db, so the engine's value and
//! id types are re-exported here.

mod catalog;
mod config;
mod convert;
mod episode;
mod error;
mod gql;
mod indexes;
mod migrate;
mod providers;
mod schema;
mod store;

pub use catalog::SCHEMA_VERSION;
pub use config::{DEFAULT_EMBEDDING_DIMENSION, StoreConfig, default_data_dir};
pub use error::StoreError;
pub use gql::{BoundQuery, QueryResult, Rows};
pub use indexes::VectorIndexInfo;
pub use migrate::{MigrationPlan, MigrationReport, PendingChange};
pub use providers::CandidateStateInfo;
pub use schema::{EdgeTypeShape, NodeTypeShape, PropertyKind, PropertyShape, SchemaSnapshot};
pub use store::Store;

pub use selene_core::{NodeId, Value};
