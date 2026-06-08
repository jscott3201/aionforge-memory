//! L0 storage over the selene-db `SharedGraph`.
//!
//! This is the only crate that names selene-db. It owns the graph, exposes typed
//! read/write that commits through the engine's single committer thread, takes
//! lock-free snapshot reads, runs caller-influenced queries through bound
//! parameters only, and translates domain values to and from selene-db property
//! maps, vectors, and JSON.
//! Higher layers depend on this crate, not on selene-db, so the engine's value and
//! id types are re-exported here.

mod audit;
mod bad_pattern;
mod catalog;
mod config;
mod consolidation;
mod convert;
mod dedup;
mod entity;
mod episode;
mod error;
mod fact;
mod gql;
mod graph_signal;
mod indexes;
mod materialize;
mod migrate;
mod note;
mod provenance;
mod providers;
mod schema;
mod search;
mod skill;
mod skill_induction;
mod store;

pub use catalog::SCHEMA_VERSION;
pub use config::{DEFAULT_EMBEDDING_DIMENSION, StoreConfig, default_data_dir};
pub use consolidation::{ConsolidationCursor, ConsolidationWorkItem, LagSnapshot};
pub use error::StoreError;
pub use gql::{BoundQuery, QueryResult, Rows};
pub use indexes::VectorIndexInfo;
pub use materialize::{
    ConsolidationArtifacts, Contradiction, FactKey, MaterializedFact, Supersession,
};
pub use migrate::{MigrationPlan, MigrationReport, PendingChange};
pub use note::MaterializedNote;
pub use providers::CandidateStateInfo;
pub use schema::{EdgeTypeShape, NodeTypeShape, PropertyKind, PropertyShape, SchemaSnapshot};
pub use search::{CandidateSet, ExpandDirection, ExpandEdge, SearchHit, SearchKind, SetOp};
pub use skill_induction::InducedSkillWrite;
pub use store::{CaptureWriteIds, Store};

pub use selene_core::{NodeId, Value};
