//! Compact MCP server status reporting.

use aionforge_engine::MemoryCounts;
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
///
/// `counts` is the live, engine-native memory census (global operator telemetry): its
/// `total()` rides the base `[server]` line as `memories=N`, and the per-kind breakdown
/// is emitted only under verbose.
#[must_use]
pub fn server_status_tool(
    resource_count: usize,
    counts: MemoryCounts,
    params: ServerStatusToolParams,
) -> String {
    let mut out = format!(
        "[server] version={} tools={} resources={} prompts={} transports={} sampling=false recall_wrapper=recalled-memory-context mutating_tools={} memories={}",
        env!("CARGO_PKG_VERSION"),
        surface::tool_count(),
        resource_count,
        surface::PROMPT_COUNT,
        surface::TRANSPORTS_COMPACT,
        surface::tool_count_by_class(ToolClass::Mutating),
        counts.total()
    );
    if params.verbose.unwrap_or(false) {
        out.push('\n');
        out.push_str("read_like_tools=");
        out.push_str(&surface::tool_names_by_class(ToolClass::ReadLike).join(","));
        out.push('\n');
        out.push_str("mutating_tools=");
        out.push_str(&surface::tool_names_by_class(ToolClass::Mutating).join(","));
        out.push('\n');
        out.push_str(&format!(
            "kinds=episodes={} facts={} entities={} notes={} skills={} bad_patterns={}",
            counts.episodes,
            counts.facts,
            counts.entities,
            counts.notes,
            counts.skills,
            counts.bad_patterns
        ));
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

    fn sample_counts() -> MemoryCounts {
        MemoryCounts {
            episodes: 3,
            facts: 2,
            entities: 1,
            notes: 0,
            skills: 0,
            bad_patterns: 0,
        }
    }

    #[test]
    fn compact_status_reports_counts_and_posture() {
        let out = server_status_tool(8, sample_counts(), ServerStatusToolParams { verbose: None });
        assert!(out.starts_with("[server] "), "{out}");
        assert!(out.contains("tools=11"), "{out}");
        assert!(out.contains("resources=8"), "{out}");
        assert!(out.contains("sampling=false"), "{out}");
        assert!(out.contains("memories=6"), "{out}");
    }

    #[test]
    fn verbose_status_lists_tool_classes() {
        let out = server_status_tool(
            8,
            sample_counts(),
            ServerStatusToolParams {
                verbose: Some(true),
            },
        );
        assert!(out.contains("read_like_tools=server_status,search,read_memory,session_manifest"));
        assert!(out.contains("mutating_tools=capture,batch_capture,consolidate,forget,unforget"));
        assert!(out.contains("aionforge://policy/tool-approval"));
        // The per-kind breakdown line is exact.
        let kinds_line = "kinds=episodes=3 facts=2 entities=1 notes=0 skills=0 bad_patterns=0";
        assert!(out.contains(kinds_line), "{out}");
        // ...and it sits between the mutating_tools roster line and the policy line.
        // Anchor to the verbose roster specifically — the base [server] line also
        // contains "mutating_tools=", so a plain find() would match that one instead.
        let mutating_at = out
            .find("mutating_tools=capture,batch_capture,consolidate,forget,unforget")
            .expect("verbose output has a mutating_tools roster line");
        let kinds_at = out
            .find(kinds_line)
            .expect("verbose output has a kinds line");
        let policy_at = out
            .find("policy=allow_read_like_ask_mutations")
            .expect("verbose output has a policy line");
        assert!(
            mutating_at < kinds_at && kinds_at < policy_at,
            "kinds line must fall between mutating_tools and policy: {out}"
        );
    }
}
