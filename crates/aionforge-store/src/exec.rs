//! The single GQL execution seam.
//!
//! Every parameter-bound statement runs through here, so the per-store shared plan
//! caches and an optional recall deadline are applied in exactly one place. The
//! deadline-aware variants let the retriever bound a slow `CALL` mid-statement instead
//! of only at its phase boundaries; the plain entry points run with no deadline.

use std::sync::Arc;
use std::time::Instant;

use selene_gql::{BindingTable, BuiltinProcedureRegistry, Session, StatementOutput};

use crate::error::StoreError;
use crate::gql::{BoundQuery, QueryResult, Rows};
use crate::store::Store;

impl Store {
    /// Execute a parameter-bound GQL statement.
    ///
    /// The query's source is fixed and trusted; every caller value travels as a
    /// bound parameter, so the parsed statement never depends on caller input.
    /// Statements run against the engine's full builtin procedure registry, so the
    /// native `CALL selene.*` / `CALL algo.*` surfaces (vector, BM25, candidate-state,
    /// and graph algorithms — 03 §1–§4) are available through this one seam.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the statement fails to parse, plan, or execute.
    pub fn execute(&self, query: &BoundQuery) -> Result<QueryResult, StoreError> {
        self.execute_within(query, None)
    }

    /// Execute a parameter-bound statement, aborting if `deadline` passes mid-statement.
    ///
    /// `None` runs with no deadline, identical to [`Store::execute`]. A `Some(deadline)`
    /// is attached to the session so a slow `CALL` cooperatively aborts at the engine's
    /// checkpoints (returning a timeout) rather than running to completion — the
    /// recall-budget path (03 §8), where the retriever otherwise only bails between
    /// phases.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the statement fails or the deadline expires.
    pub(crate) fn execute_within(
        &self,
        query: &BoundQuery,
        deadline: Option<Instant>,
    ) -> Result<QueryResult, StoreError> {
        let mut session = self.new_session(deadline);
        for (name, value) in query.params() {
            session.bind_parameter(name.clone(), value.clone());
        }
        let registry = BuiltinProcedureRegistry::new();
        let output = session.execute_source(query.source(), &registry)?;
        materialize(output)
    }

    /// Run a sequence of parameter-bound statements in one engine session, returning the
    /// last statement's result, optionally bounded by a mid-statement `deadline`.
    ///
    /// Unlike [`Store::execute`] — which opens a fresh session per call — every statement
    /// here shares one session. selene graph-algorithm projections live in the session,
    /// not the graph, so a projection a `CALL algo.projection_build` statement registers
    /// is visible to a later `CALL algo.pagerank` over it; running them through two
    /// separate `execute` calls would lose the projection between them. Each source is
    /// fixed and trusted and every caller value travels as a bound parameter. `deadline`
    /// of `None` runs without a deadline.
    ///
    /// # Errors
    /// Returns [`StoreError`] if any statement fails to parse, plan, or execute, or the
    /// deadline expires.
    pub(crate) fn execute_session_within(
        &self,
        statements: &[BoundQuery],
        deadline: Option<Instant>,
    ) -> Result<QueryResult, StoreError> {
        let registry = BuiltinProcedureRegistry::new();
        let mut session = self.new_session(deadline);
        let mut result = QueryResult::Empty;
        for query in statements {
            for (name, value) in query.params() {
                session.bind_parameter(name.clone(), value.clone());
            }
            result = materialize(session.execute_source(query.source(), &registry)?)?;
        }
        Ok(result)
    }

    /// Open a session over the graph with the shared plan caches and an optional deadline.
    fn new_session(&self, deadline: Option<Instant>) -> Session<'_> {
        let session = Session::new(self.graph())
            .with_shared_plan_cache(Arc::clone(&self.shared_plan_cache))
            .with_call_plan_cache(Arc::clone(&self.call_plan_cache));
        match deadline {
            Some(deadline) => session.with_deadline(deadline),
            None => session,
        }
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
