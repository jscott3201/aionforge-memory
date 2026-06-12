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
    pub(crate) read_only_hint: bool,
    pub(crate) destructive_hint: bool,
    pub(crate) idempotent_hint: bool,
    pub(crate) open_world_hint: bool,
    pub(crate) errors: &'static [&'static str],
}

pub(crate) const TOOLS: &[ToolSurface] = &[
    ToolSurface {
        name: "server_status",
        class: ToolClass::ReadLike,
        default_output: "one compact [server] status line",
        verbose: true,
        read_only_hint: true,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: false,
        errors: &[],
    },
    ToolSurface {
        name: "search",
        class: ToolClass::ReadLike,
        default_output: "compact recalled-memory-context wrapper with one line per hit",
        verbose: true,
        read_only_hint: true,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: false,
        errors: &[
            "ERR_MISSING_PRINCIPAL",
            "ERR_INVALID_VIEWER",
            "ERR_INVALID_AGENT_ID",
            "ERR_PRINCIPAL_MISMATCH",
            "ERR_SEARCH",
        ],
    },
    ToolSurface {
        name: "read_memory",
        class: ToolClass::ReadLike,
        default_output: "one visible memory by id in a recalled-memory-context wrapper",
        verbose: true,
        read_only_hint: true,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: false,
        errors: &[
            "ERR_INVALID_MEMORY_ID",
            "ERR_MISSING_PRINCIPAL",
            "ERR_INVALID_VIEWER",
            "ERR_INVALID_AGENT_ID",
            "ERR_PRINCIPAL_MISMATCH",
            "ERR_NOT_FOUND",
            "ERR_READ_MEMORY",
        ],
    },
    ToolSurface {
        name: "session_manifest",
        class: ToolClass::ReadLike,
        default_output: "visible captured-memory manifest for one session",
        verbose: true,
        read_only_hint: true,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: false,
        errors: &[
            "ERR_INVALID_SESSION_ID",
            "ERR_INVALID_SESSION_CURSOR",
            "ERR_INVALID_SESSION_CURSOR_ID",
            "ERR_MISSING_PRINCIPAL",
            "ERR_INVALID_VIEWER",
            "ERR_INVALID_AGENT_ID",
            "ERR_PRINCIPAL_MISMATCH",
            "ERR_SESSION_MANIFEST",
        ],
    },
    ToolSurface {
        name: "consolidation_status",
        class: ToolClass::ReadLike,
        default_output: "one compact [consolidation] backlog line",
        verbose: true,
        read_only_hint: true,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: false,
        errors: &["ERR_CONSOLIDATION_STATUS"],
    },
    ToolSurface {
        name: "audit_history",
        class: ToolClass::ReadLike,
        default_output: "compact audit page with cursor; subject=* means all visible subjects for a kind",
        verbose: true,
        read_only_hint: true,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: false,
        errors: &[
            "ERR_INVALID_SUBJECT_ID",
            "ERR_INVALID_AUDIT_QUERY",
            "ERR_MISSING_PRINCIPAL",
            "ERR_INVALID_VIEWER",
            "ERR_INVALID_AGENT_ID",
            "ERR_PRINCIPAL_MISMATCH",
            "ERR_INVALID_AUDIT_CURSOR",
            "ERR_INVALID_AUDIT_CURSOR_ID",
            "ERR_INVALID_AUDIT_KIND",
            "ERR_AUDIT_HISTORY",
        ],
    },
    ToolSurface {
        name: "capture",
        class: ToolClass::Mutating,
        default_output: "one compact [capture] receipt line with marker ids when flags fire",
        verbose: false,
        read_only_hint: false,
        destructive_hint: false,
        idempotent_hint: false,
        open_world_hint: false,
        errors: &[
            "ERR_MISSING_AGENT_ID",
            "ERR_INVALID_AGENT_ID",
            "ERR_PRINCIPAL_MISMATCH",
            "ERR_INVALID_SESSION_ID",
            "ERR_INVALID_ROLE",
            "ERR_INVALID_CAPTURED_AT",
            "ERR_INVALID_TARGET_NAMESPACE",
            "ERR_CAPTURE",
        ],
    },
    ToolSurface {
        name: "consolidate",
        class: ToolClass::Mutating,
        default_output: "one compact [consolidate] run summary line",
        verbose: true,
        read_only_hint: false,
        destructive_hint: false,
        idempotent_hint: false,
        open_world_hint: false,
        errors: &["ERR_CONSOLIDATE_BUSY", "ERR_CONSOLIDATE"],
    },
    ToolSurface {
        name: "forget",
        class: ToolClass::Mutating,
        default_output: "one compact [forget] outcome line with disabled config reason when off",
        verbose: false,
        read_only_hint: false,
        destructive_hint: true,
        idempotent_hint: true,
        open_world_hint: false,
        errors: &[
            "ERR_INVALID_MEMORY_ID",
            "ERR_MISSING_PRINCIPAL",
            "ERR_INVALID_VIEWER",
            "ERR_INVALID_AGENT_ID",
            "ERR_PRINCIPAL_MISMATCH",
            "ERR_LOOKUP",
            "ERR_NOT_FOUND",
            "ERR_FORGET",
        ],
    },
    ToolSurface {
        name: "unforget",
        class: ToolClass::Mutating,
        default_output: "one compact [unforget] outcome line with disabled config reason when off",
        verbose: false,
        read_only_hint: false,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: false,
        errors: &[
            "ERR_INVALID_MEMORY_ID",
            "ERR_MISSING_PRINCIPAL",
            "ERR_INVALID_VIEWER",
            "ERR_INVALID_AGENT_ID",
            "ERR_PRINCIPAL_MISMATCH",
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
