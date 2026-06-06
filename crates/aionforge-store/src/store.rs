//! The L0 store: a single owned `SharedGraph` with typed read/write and
//! parameter-bound GQL.

use std::sync::Arc;

use aionforge_domain::nodes::episodic::Episode;
use selene_core::{GraphId, NodeId};
use selene_gql::{BindingTable, EmptyProcedureRegistry, Session, StatementOutput};
use selene_graph::{SeleneGraph, SharedGraph};

use crate::episode;
use crate::error::StoreError;
use crate::gql::{BoundQuery, QueryResult, Rows};

/// The storage layer over a selene-db `SharedGraph`.
///
/// Owns one graph for the process lifetime (constructing it spawns the engine's
/// single committer thread, which is the sole snapshot publisher). Every write —
/// the typed [`Store::insert_episode`] path and any data-modifying statement the
/// engine auto-commits inside [`Store::execute`] — commits through that one
/// committer, durable before visible. Reads take a lock-free snapshot. Every
/// caller-influenced value travels as a bound parameter, never spliced into the
/// query text.
pub struct Store {
    graph: SharedGraph,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // SharedGraph is not Debug, so we do not recurse into it.
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// Open an in-memory store with no persistence (the M0 / test path).
    ///
    /// Persistence (WAL, snapshots, recovery) is wired in a later task; this opens
    /// a bare graph that holds everything in memory.
    ///
    /// # Errors
    /// Currently infallible, but returns [`StoreError`] so the persistent
    /// constructor can share the signature.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Ok(Self {
            graph: SharedGraph::new(GraphId::new(1)),
        })
    }

    /// Take a lock-free read snapshot of the current graph state.
    ///
    /// The returned `Arc` pins that snapshot version; drop it promptly (per
    /// statement) so superseded snapshots are reclaimed.
    #[must_use]
    pub fn snapshot(&self) -> Arc<SeleneGraph> {
        self.graph.read()
    }

    /// Commit an episode through the single write funnel, returning its node id.
    ///
    /// # Errors
    /// Returns [`StoreError`] if translation, the mutation, or the commit fails.
    pub fn insert_episode(&self, episode: &Episode) -> Result<NodeId, StoreError> {
        let (labels, props) = episode::to_node(episode)?;
        let mut txn = self.graph.begin_write();
        let node_id = {
            let mut mutator = txn.mutator();
            mutator.create_node(labels, props)?
        };
        txn.commit()?;
        Ok(node_id)
    }

    /// Read an episode back by its node id from a fresh snapshot.
    ///
    /// Returns `Ok(None)` if no live node with that id exists.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the stored data cannot be decoded into an
    /// [`Episode`].
    pub fn episode_by_node_id(&self, id: NodeId) -> Result<Option<Episode>, StoreError> {
        let snapshot = self.graph.read();
        match snapshot.node_properties(id) {
            Some(props) => Ok(Some(episode::from_properties(props)?)),
            None => Ok(None),
        }
    }

    /// Execute a parameter-bound GQL statement.
    ///
    /// The query's source is fixed and trusted; every caller value travels as a
    /// bound parameter, so the parsed statement never depends on caller input.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the statement fails to parse, plan, or execute.
    pub fn execute(&self, query: &BoundQuery) -> Result<QueryResult, StoreError> {
        let mut session = Session::new(&self.graph);
        for (name, value) in query.params() {
            session.bind_parameter(name.clone(), value.clone());
        }
        let output = session.execute_source(query.source(), &EmptyProcedureRegistry)?;
        materialize(output)
    }
}

/// Convert an owned engine [`StatementOutput`] into the owned [`QueryResult`].
///
/// A data-modifying statement carrying a `RETURN` auto-commits and still yields
/// rows; those are carried through on [`QueryResult::Written`] rather than dropped.
fn materialize(output: StatementOutput) -> Result<QueryResult, StoreError> {
    match output {
        StatementOutput::Empty => Ok(QueryResult::Empty),
        StatementOutput::Written(outcome) => Ok(QueryResult::Written {
            generation: outcome.generation,
            rows: outcome.rows.map(materialize_table),
        }),
        StatementOutput::Rows(table) => Ok(QueryResult::Rows(materialize_table(table))),
        other => Err(StoreError::decode(format!(
            "unrecognized statement output: {other:?}"
        ))),
    }
}

/// Materialize an owned engine binding table into the owned [`Rows`].
fn materialize_table(table: BindingTable) -> Rows {
    let columns = table
        .schema()
        .columns
        .iter()
        .map(|column| column.name.as_ref().map(|name| name.as_str().to_string()))
        .collect();
    let rows = table
        .iter()
        .map(|binding| binding.values().to_vec())
        .collect();
    Rows::new(columns, rows)
}
