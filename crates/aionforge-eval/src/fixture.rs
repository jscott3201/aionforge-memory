//! The labeled evaluation fixture: a small synthetic corpus plus positive and negative
//! queries.
//!
//! Rows are JSONL (one JSON object per line). A **memory** row is a synthetic note; a
//! **query** row is either a positive (carrying graded relevance labels over the memory
//! ids) or a negative (an off-topic query whose correct answer is empty). The fixture
//! ids are joined to the seeded store ids by the runner, which records a map from each
//! fixture id to the domain id it generated — so labels never depend on text equality.

use serde::Deserialize;

/// A synthetic memory row to seed into the store.
#[derive(Debug, Clone, Deserialize)]
pub struct MemoryRow {
    /// Stable fixture id (e.g. `m-0001`), joined to the seeded domain id by the runner.
    pub id: String,
    /// The memory text, embedded by the real embedder and stored verbatim.
    pub text: String,
    /// Effective importance in `[0, 1]`.
    pub importance: f64,
    /// Writer trust in `[0, 1]`.
    pub trust: f64,
    /// Provenance tag asserted on every row (a synthetic-corpus marker).
    pub source: String,
}

/// A graded relevance label: a memory id and how relevant it is to a query.
#[derive(Debug, Clone, Deserialize)]
pub struct Graded {
    /// The fixture id of the relevant memory.
    pub id: String,
    /// Relevance grade (`0` = irrelevant, higher = more relevant); gain is `2^grade - 1`.
    pub grade: u8,
}

/// A query row: a positive query with graded labels, or a negative (off-topic) query.
#[derive(Debug, Clone, Deserialize)]
pub struct QueryRow {
    /// Stable fixture id (e.g. `q-0001`).
    pub id: String,
    /// The natural-language query text.
    pub query: String,
    /// Provenance tag asserted on every row.
    pub source: String,
    /// `true` for an off-topic query whose correct answer is empty (a negative). A
    /// negative carries no `expected` labels and is scored by rejection, not recall.
    #[serde(default)]
    pub expected_empty: bool,
    /// Graded relevance labels for a positive query (empty for a negative).
    #[serde(default)]
    pub expected: Vec<Graded>,
}

impl QueryRow {
    /// Whether this is a negative (off-topic) query whose correct answer is empty.
    #[must_use]
    pub fn is_negative(&self) -> bool {
        self.expected_empty
    }

    /// The fixture ids of this query's relevant (grade > 0) memories.
    #[must_use]
    pub fn gold_ids(&self) -> Vec<&str> {
        self.expected
            .iter()
            .filter(|graded| graded.grade > 0)
            .map(|graded| graded.id.as_str())
            .collect()
    }
}

/// Parse memory rows from a JSONL fixture.
///
/// # Errors
/// Returns the first JSON parse error encountered.
pub fn parse_memories(jsonl: &str) -> serde_json::Result<Vec<MemoryRow>> {
    parse_jsonl(jsonl)
}

/// Parse query rows from a JSONL fixture.
///
/// # Errors
/// Returns the first JSON parse error encountered.
pub fn parse_queries(jsonl: &str) -> serde_json::Result<Vec<QueryRow>> {
    parse_jsonl(jsonl)
}

fn parse_jsonl<T: serde::de::DeserializeOwned>(input: &str) -> serde_json::Result<Vec<T>> {
    input
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_positive_and_a_negative_query() {
        let jsonl = concat!(
            r#"{"id":"q-1","query":"how to compost","source":"syn","expected":[{"id":"m-1","grade":3},{"id":"m-2","grade":1}]}"#,
            "\n",
            r#"{"id":"q-2","query":"roman empire history","source":"syn","expected_empty":true}"#,
            "\n"
        );
        let rows = parse_queries(jsonl).expect("parse");
        assert_eq!(rows.len(), 2);
        assert!(!rows[0].is_negative());
        assert_eq!(rows[0].gold_ids(), vec!["m-1", "m-2"]);
        assert!(rows[1].is_negative());
        assert!(rows[1].gold_ids().is_empty());
    }

    #[test]
    fn parses_memory_rows() {
        let jsonl =
            r#"{"id":"m-1","text":"turn the compost","importance":0.5,"trust":0.8,"source":"syn"}"#;
        let rows = parse_memories(jsonl).expect("parse");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "m-1");
        assert!((rows[0].trust - 0.8).abs() < 1e-12);
    }

    #[test]
    fn a_blank_line_is_skipped() {
        let rows = parse_memories("\n\n").expect("parse");
        assert!(rows.is_empty());
    }
}
