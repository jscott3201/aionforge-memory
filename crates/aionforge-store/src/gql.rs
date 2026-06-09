//! Parameter-bound GQL: the only sanctioned way L0 runs caller-influenced queries.
//!
//! [`BoundQuery`] separates a fixed, trusted source string from the caller values,
//! which are carried as bound parameters and never spliced into the text. There is
//! no method that interpolates a value into the source, so injection is structurally
//! impossible — the `check-no-gql-interpolation` gate backstops it in review.

use selene_core::{DbString, Value, db_string};

use crate::error::StoreError;

/// A GQL statement plus its bound parameters.
///
/// Reference a parameter in the source as `$name`; bind it here under `name` (no
/// leading `$`). Re-binding the same name replaces the prior value.
#[derive(Debug, Clone)]
pub struct BoundQuery {
    source: String,
    params: Vec<(DbString, Value)>,
}

impl BoundQuery {
    /// Start a query from a fixed source string.
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            params: Vec::new(),
        }
    }

    /// Bind a parameter to a value.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the name exceeds the engine's string cap.
    pub fn bind(mut self, name: &str, value: Value) -> Result<Self, StoreError> {
        self.params.push((db_string(name)?, value));
        Ok(self)
    }

    /// Bind a parameter to a string value (convenience over [`BoundQuery::bind`]).
    ///
    /// # Errors
    /// Returns [`StoreError`] if the name or value exceeds the engine's string cap.
    pub fn bind_str(self, name: &str, value: &str) -> Result<Self, StoreError> {
        let v = Value::String(db_string(value)?);
        self.bind(name, v)
    }

    /// Bind a parameter to an [`Id`](aionforge_domain::ids::Id) as a native UUID value (convenience over
    /// [`BoundQuery::bind`]). Use this for id-equality filters so the bound value's
    /// type matches the UUID-typed id columns.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the parameter name exceeds the engine's string cap.
    pub fn bind_uuid(
        self,
        name: &str,
        id: impl std::borrow::Borrow<aionforge_domain::Id>,
    ) -> Result<Self, StoreError> {
        self.bind(name, Value::Uuid(id.borrow().as_uuid()))
    }

    /// Bind a parameter to a [`Timestamp`](aionforge_domain::time::Timestamp) as a native
    /// `ZONED DATETIME` value (convenience over [`BoundQuery::bind`]). Lets a caller that cannot
    /// name the engine's value type set a temporal edge or node property from a domain timestamp.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the parameter name exceeds the engine's string cap.
    pub fn bind_timestamp(
        self,
        name: &str,
        at: &aionforge_domain::time::Timestamp,
    ) -> Result<Self, StoreError> {
        self.bind(name, crate::convert::timestamp_value(at))
    }

    pub(crate) fn source(&self) -> &str {
        &self.source
    }

    pub(crate) fn params(&self) -> &[(DbString, Value)] {
        &self.params
    }
}

/// The owned result of executing a [`BoundQuery`].
#[derive(Debug, Clone)]
pub enum QueryResult {
    /// A statement with no row-bearing result (a write with no `RETURN`, DDL, …).
    Empty,
    /// An auto-committed write that published a new graph generation.
    Written {
        /// The published graph generation after the write.
        generation: u64,
        /// Rows projected by a `RETURN` on the write, if the statement had one.
        rows: Option<Rows>,
    },
    /// A read query's result rows.
    Rows(Rows),
}

/// A materialized, owned result table.
#[derive(Debug, Clone)]
pub struct Rows {
    columns: Vec<Option<String>>,
    rows: Vec<Vec<Value>>,
}

impl Rows {
    pub(crate) fn new(columns: Vec<Option<String>>, rows: Vec<Vec<Value>>) -> Self {
        Self { columns, rows }
    }

    /// The column names, in order. A name is `None` for an anonymous column.
    #[must_use]
    pub fn columns(&self) -> &[Option<String>] {
        &self.columns
    }

    /// The number of result rows.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// True when there are no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The rows, each a vector of column values.
    #[must_use]
    pub fn rows(&self) -> &[Vec<Value>] {
        &self.rows
    }

    /// The index of the first column named `name`, if any.
    #[must_use]
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.as_deref() == Some(name))
    }

    /// The value at `(row, column)`, if both indices are in range.
    #[must_use]
    pub fn value(&self, row: usize, column: usize) -> Option<&Value> {
        self.rows.get(row).and_then(|r| r.get(column))
    }
}
