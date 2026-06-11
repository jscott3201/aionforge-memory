//! Compact MCP server status reporting.

use schemars::JsonSchema;
use serde::Deserialize;

use crate::surface::{self, ToolClass};

/// Parameters for the `server_status` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ServerStatusToolParams {
    /// Include tool class lists and operational hints.
    #[schemars(description = "Include tool class lists and operational hints.")]
    pub verbose: Option<bool>,
}

/// Render compact MCP server status.
#[must_use]
pub fn server_status_tool(resource_count: usize, params: ServerStatusToolParams) -> String {
    let mut out = format!(
        "[server] version={} tools={} resources={} prompts={} transports={} sampling=false recall_wrapper=recalled-memory-context mutating_tools={}",
        env!("CARGO_PKG_VERSION"),
        surface::tool_count(),
        resource_count,
        surface::PROMPT_COUNT,
        surface::TRANSPORTS_COMPACT,
        surface::tool_count_by_class(ToolClass::Mutating)
    );
    if params.verbose.unwrap_or(false) {
        out.push('\n');
        out.push_str("read_like_tools=");
        out.push_str(&surface::tool_names_by_class(ToolClass::ReadLike).join(","));
        out.push('\n');
        out.push_str("mutating_tools=");
        out.push_str(&surface::tool_names_by_class(ToolClass::Mutating).join(","));
        out.push('\n');
        out.push_str(
            "policy=allow_read_like_ask_mutations resources=aionforge://manifest/tools.json,aionforge://guide/mcp-surface,aionforge://policy/tool-approval",
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_status_reports_counts_and_posture() {
        let out = server_status_tool(8, ServerStatusToolParams { verbose: None });
        assert!(out.starts_with("[server] "), "{out}");
        assert!(out.contains("tools=8"), "{out}");
        assert!(out.contains("resources=8"), "{out}");
        assert!(out.contains("sampling=false"), "{out}");
    }

    #[test]
    fn verbose_status_lists_tool_classes() {
        let out = server_status_tool(
            8,
            ServerStatusToolParams {
                verbose: Some(true),
            },
        );
        assert!(out.contains("read_like_tools=server_status,search"));
        assert!(out.contains("mutating_tools=capture,consolidate"));
        assert!(out.contains("aionforge://policy/tool-approval"));
    }
}
