//! Helpers for MCP tool results that carry both legacy text and structured JSON.

use rmcp::model::{CallToolResult, Content};
use serde::Serialize;
use serde_json::Value;

pub(crate) mod inspect;
pub(crate) mod search;

/// A tool result split into the stable text payload and the typed `structuredContent` payload.
pub(crate) struct StructuredToolOutput {
    /// Existing text output, preserved for agent clients and compatibility tests.
    pub(crate) text: String,
    /// JSON value attached to the MCP `structuredContent` field.
    pub(crate) structured: Value,
}

impl StructuredToolOutput {
    /// Build a split output from a serializable DTO.
    pub(crate) fn new<T: Serialize>(text: String, structured: T) -> Self {
        Self {
            text,
            structured: serde_json::to_value(structured).expect("structured DTO serializes"),
        }
    }
}

/// Convert a split output into the rmcp transport result shape.
pub(crate) fn call_tool_result(output: StructuredToolOutput) -> CallToolResult {
    let mut result = CallToolResult::success(vec![Content::text(output.text)]);
    result.structured_content = Some(output.structured);
    result
}
