//! The typed L0 error space.

use aionforge_domain::DomainError;
use selene_core::CoreError;
use selene_gql::ExecutorError;

/// An error from the L0 storage layer.
///
/// Wraps the three selene-db error families (graph mutation, core value
/// construction, GQL execution) plus the domain error space and the
/// translation/decode failures that only L0 can surface. The selene errors carry
/// their own [`miette`] diagnostics through transparently.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[non_exhaustive]
pub enum StoreError {
    /// A graph write, commit, or schema operation failed.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Graph(#[from] selene_graph::GraphError),

    /// Constructing a selene-db value (string, vector, JSON, property map) failed.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Core(#[from] CoreError),

    /// A parameter-bound GQL statement failed to parse, plan, or execute.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Gql(#[from] ExecutorError),

    /// A domain value rejected a stored value on read-back (bad id, namespace, …).
    #[error("domain value error: {0}")]
    Domain(#[from] DomainError),

    /// A JSON-typed property failed to serialize or deserialize.
    #[error("JSON translation failed: {0}")]
    Json(#[from] serde_json::Error),

    /// Stored graph data did not match the shape a domain kind expects.
    #[error("could not decode stored data into a domain value: {0}")]
    Decode(String),

    /// A filesystem operation on the store's on-disk state failed (creating the
    /// data directory, for instance). WAL-open and commit failures surface as
    /// [`StoreError::Graph`] instead — this covers the store's own I/O.
    #[error("persistence error: {0}")]
    Persist(String),

    /// A native-search primitive was asked for something it cannot serve, such as a
    /// text search on a kind that maintains no text index.
    #[error("search error: {0}")]
    Search(String),

    /// A write was rejected at the boundary because it would violate a domain
    /// invariant — for instance a bi-temporal window whose bounds are out of order.
    /// The write funnel fails closed rather than persist inconsistent state.
    #[error("invariant violation: {0}")]
    Invariant(String),
}

impl StoreError {
    /// Construct a [`StoreError::Decode`] from a message.
    pub(crate) fn decode(message: impl Into<String>) -> Self {
        Self::Decode(message.into())
    }

    /// Construct a [`StoreError::Persist`] from a message.
    pub(crate) fn persist(message: impl Into<String>) -> Self {
        Self::Persist(message.into())
    }

    /// Construct a [`StoreError::Search`] from a message.
    pub(crate) fn search(message: impl Into<String>) -> Self {
        Self::Search(message.into())
    }

    /// Construct a [`StoreError::Invariant`] from a message.
    pub(crate) fn invariant(message: impl Into<String>) -> Self {
        Self::Invariant(message.into())
    }
}
