//! L0 storage over the selene-db `SharedGraph`.
//!
//! This is the only crate that names selene-db. It owns the graph, exposes typed
//! read/write that commits through the engine's single committer thread, takes
//! lock-free snapshot reads, runs caller-influenced queries through bound
//! parameters only, and translates domain values to and from selene-db property
//! maps, vectors, and JSON.
//! Higher layers depend on this crate, not on selene-db, so the engine's value and
//! id types are re-exported here.

mod convert;
mod episode;
mod error;
mod gql;
mod store;

pub use error::StoreError;
pub use gql::{BoundQuery, QueryResult, Rows};
pub use store::Store;

pub use selene_core::{NodeId, Value};
