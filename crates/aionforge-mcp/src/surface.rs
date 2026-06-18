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
        default_output: "[server] status line",
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
        default_output: "hits header plus recalled-memory-context lines",
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
        default_output: "[read_memory] requested/found plus visible memory lines",
        verbose: true,
        read_only_hint: true,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: false,
        errors: &[
            "ERR_NO_MEMORY_IDS",
            "ERR_TOO_MANY_IDS",
            "ERR_INVALID_MEMORY_ID",
            "ERR_MISSING_PRINCIPAL",
            "ERR_INVALID_VIEWER",
            "ERR_INVALID_AGENT_ID",
            "ERR_PRINCIPAL_MISMATCH",
            "ERR_READ_MEMORY",
        ],
    },
    ToolSurface {
        name: "session_manifest",
        class: ToolClass::ReadLike,
        default_output: "[session_manifest] page plus visible episode lines",
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
        default_output: "[consolidation] backlog line",
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
        default_output: "[audit] page with optional cursor",
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
        default_output: "[capture] receipt line",
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
            "ERR_INVALID_SUPERSEDES",
            "ERR_CAPTURE",
        ],
    },
    ToolSurface {
        name: "batch_capture",
        class: ToolClass::Mutating,
        default_output: "[batch_capture] tally plus per-item receipts/errors",
        verbose: false,
        read_only_hint: false,
        destructive_hint: false,
        idempotent_hint: false,
        open_world_hint: false,
        errors: &[
            "ERR_EMPTY_BATCH",
            "ERR_BATCH_TOO_LARGE",
            "ERR_MISSING_AGENT_ID",
            "ERR_INVALID_AGENT_ID",
            "ERR_PRINCIPAL_MISMATCH",
            "ERR_INVALID_SESSION_ID",
            "ERR_INVALID_ROLE",
            "ERR_INVALID_CAPTURED_AT",
            "ERR_INVALID_TARGET_NAMESPACE",
            "ERR_INVALID_SUPERSEDES",
            "ERR_CAPTURE",
        ],
    },
    ToolSurface {
        name: "consolidate",
        class: ToolClass::Mutating,
        default_output: "[consolidate] run summary",
        verbose: true,
        read_only_hint: false,
        destructive_hint: false,
        idempotent_hint: false,
        open_world_hint: false,
        errors: &[
            "ERR_CONSOLIDATE_MANAGED",
            "ERR_CONSOLIDATE_BUSY",
            "ERR_CONSOLIDATE",
        ],
    },
    ToolSurface {
        name: "forget",
        class: ToolClass::Mutating,
        default_output: "[forget] outcome line",
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
        default_output: "[unforget] outcome line",
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
    ToolSurface {
        name: "pin",
        class: ToolClass::Mutating,
        default_output: "[pin] outcome line",
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
            "ERR_PIN",
        ],
    },
    ToolSurface {
        name: "unpin",
        class: ToolClass::Mutating,
        default_output: "[unpin] outcome line",
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
            "ERR_UNPIN",
        ],
    },
    ToolSurface {
        name: "work_create",
        class: ToolClass::Mutating,
        default_output: "[work_create] receipt line",
        verbose: false,
        read_only_hint: false,
        destructive_hint: false,
        idempotent_hint: false,
        open_world_hint: false,
        errors: &[
            "ERR_READ_ONLY_PRINCIPAL",
            "ERR_MISSING_PRINCIPAL",
            "ERR_INVALID_VIEWER",
            "ERR_INVALID_AGENT_ID",
            "ERR_PRINCIPAL_MISMATCH",
            "ERR_INVALID_TARGET_NAMESPACE",
            "ERR_NOT_AUTHORIZED",
            "ERR_INVALID_WORK_ID",
            "ERR_LOOKUP",
            "ERR_WORK_PARENT_NOT_FOUND",
            "ERR_WORK_PARENT_NAMESPACE",
            "ERR_WORK_SAVE",
        ],
    },
    ToolSurface {
        name: "work_advance",
        class: ToolClass::Mutating,
        default_output: "[work_advance] outcome line",
        verbose: false,
        read_only_hint: false,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: false,
        errors: &[
            "ERR_INVALID_WORK_ID",
            "ERR_INVALID_WORK_STATUS",
            "ERR_MISSING_PRINCIPAL",
            "ERR_INVALID_VIEWER",
            "ERR_INVALID_AGENT_ID",
            "ERR_PRINCIPAL_MISMATCH",
            "ERR_LOOKUP",
            "ERR_NOT_FOUND",
            "ERR_WORK_STATE_CONFLICT",
            "ERR_WORK_ADVANCE",
        ],
    },
    ToolSurface {
        name: "work_link",
        class: ToolClass::Mutating,
        default_output: "[work_link] outcome line",
        verbose: false,
        read_only_hint: false,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: false,
        errors: &[
            "ERR_INVALID_WORK_ID",
            "ERR_INVALID_SLUG",
            "ERR_MISSING_PRINCIPAL",
            "ERR_INVALID_VIEWER",
            "ERR_INVALID_AGENT_ID",
            "ERR_PRINCIPAL_MISMATCH",
            "ERR_LOOKUP",
            "ERR_NOT_FOUND",
            "ERR_WORK_LINK",
        ],
    },
    ToolSurface {
        name: "work_tree",
        class: ToolClass::ReadLike,
        default_output: "[work_tree] root/found plus visible work lines",
        verbose: false,
        read_only_hint: true,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: false,
        errors: &[
            "ERR_INVALID_WORK_ID",
            "ERR_MISSING_PRINCIPAL",
            "ERR_INVALID_VIEWER",
            "ERR_INVALID_AGENT_ID",
            "ERR_PRINCIPAL_MISMATCH",
            "ERR_WORK_TREE",
        ],
    },
    ToolSurface {
        name: "work_query",
        class: ToolClass::ReadLike,
        default_output: "[work_query] filter/found plus visible work lines",
        verbose: false,
        read_only_hint: true,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: false,
        errors: &[
            "ERR_INVALID_WORK_STATUS",
            "ERR_MISSING_PRINCIPAL",
            "ERR_INVALID_VIEWER",
            "ERR_INVALID_AGENT_ID",
            "ERR_PRINCIPAL_MISMATCH",
            "ERR_WORK_QUERY",
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
