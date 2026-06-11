//! Shared metadata for the user-facing MCP surface.

/// Public server name used in client setup snippets.
pub(crate) const SERVER_NAME: &str = "aionforge-memory";
/// Compact transport list used in one-line status output.
pub(crate) const TRANSPORTS_COMPACT: &str = "stdio,streamable_http";
/// Transports the server surface supports.
pub(crate) const TRANSPORTS: &[&str] = &["stdio", "streamable_http"];
/// Number of prompts currently advertised by the server.
pub(crate) const PROMPT_COUNT: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolClass {
    ReadLike,
    Mutating,
}

impl ToolClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ReadLike => "read_like",
            Self::Mutating => "mutating",
        }
    }

    pub(crate) fn approval(self) -> &'static str {
        match self {
            Self::ReadLike => "allow_without_prompt",
            Self::Mutating => "ask_user",
        }
    }

    pub(crate) fn mutates(self) -> bool {
        matches!(self, Self::Mutating)
    }
}

pub(crate) struct ToolSurface {
    pub(crate) name: &'static str,
    pub(crate) class: ToolClass,
    pub(crate) default_output: &'static str,
    pub(crate) verbose: bool,
    pub(crate) errors: &'static [&'static str],
}

pub(crate) const TOOLS: &[ToolSurface] = &[
    ToolSurface {
        name: "server_status",
        class: ToolClass::ReadLike,
        default_output: "one compact [server] status line",
        verbose: true,
        errors: &[],
    },
    ToolSurface {
        name: "search",
        class: ToolClass::ReadLike,
        default_output: "compact recalled-memory-context wrapper with one line per hit",
        verbose: true,
        errors: &["ERR_INVALID_VIEWER", "ERR_SEARCH"],
    },
    ToolSurface {
        name: "consolidation_status",
        class: ToolClass::ReadLike,
        default_output: "one compact [consolidation] backlog line",
        verbose: true,
        errors: &["ERR_CONSOLIDATION_STATUS"],
    },
    ToolSurface {
        name: "audit_history",
        class: ToolClass::ReadLike,
        default_output: "compact audit page with cursor",
        verbose: true,
        errors: &[
            "ERR_INVALID_SUBJECT_ID",
            "ERR_INVALID_VIEWER",
            "ERR_INVALID_AUDIT_CURSOR",
            "ERR_INVALID_AUDIT_CURSOR_ID",
            "ERR_INVALID_AUDIT_KIND",
            "ERR_AUDIT_HISTORY",
        ],
    },
    ToolSurface {
        name: "capture",
        class: ToolClass::Mutating,
        default_output: "one compact [capture] receipt line",
        verbose: false,
        errors: &[
            "ERR_INVALID_AGENT_ID",
            "ERR_INVALID_SESSION_ID",
            "ERR_INVALID_ROLE",
            "ERR_INVALID_CAPTURED_AT",
            "ERR_CAPTURE",
        ],
    },
    ToolSurface {
        name: "consolidate",
        class: ToolClass::Mutating,
        default_output: "one compact [consolidate] run summary line",
        verbose: true,
        errors: &["ERR_CONSOLIDATE_BUSY", "ERR_CONSOLIDATE"],
    },
    ToolSurface {
        name: "forget",
        class: ToolClass::Mutating,
        default_output: "one compact [forget] outcome line",
        verbose: false,
        errors: &[
            "ERR_INVALID_MEMORY_ID",
            "ERR_INVALID_VIEWER",
            "ERR_LOOKUP",
            "ERR_NOT_FOUND",
            "ERR_FORGET",
        ],
    },
    ToolSurface {
        name: "unforget",
        class: ToolClass::Mutating,
        default_output: "one compact [unforget] outcome line",
        verbose: false,
        errors: &[
            "ERR_INVALID_MEMORY_ID",
            "ERR_INVALID_VIEWER",
            "ERR_LOOKUP",
            "ERR_NOT_FOUND",
            "ERR_UNFORGET",
        ],
    },
];

pub(crate) fn tool_count() -> usize {
    TOOLS.len()
}

pub(crate) fn tool_names_by_class(class: ToolClass) -> Vec<&'static str> {
    TOOLS
        .iter()
        .filter(|tool| tool.class == class)
        .map(|tool| tool.name)
        .collect()
}

pub(crate) fn tool_count_by_class(class: ToolClass) -> usize {
    TOOLS.iter().filter(|tool| tool.class == class).count()
}
