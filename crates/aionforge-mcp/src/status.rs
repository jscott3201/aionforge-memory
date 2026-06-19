//! Compact MCP server status reporting.

use aionforge_engine::{MemoryCounts, WorkCounts};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::resources::{
    MCP_SURFACE_GUIDE_RESOURCE_URI, TOOL_APPROVAL_POLICY_RESOURCE_URI, TOOL_MANIFEST_RESOURCE_URI,
};
use crate::structured::StructuredToolOutput;
use crate::surface::{self, ToolClass};
use crate::traffic::{TOKEN_ESTIMATE_BYTES_PER_TOKEN, TrafficSnapshot};

/// The short source SHA baked in at build time (release-integrity Layer 1, see `build.rs`).
/// `option_env!` degrades to `unknown` for a build without git rather than failing to
/// compile. This is a self-declared advertisement; the cryptographic proof a release matches
/// this SHA is the SLSA attestation published by the release pipeline (PR #202), verified
/// out of band — not this string.
const BUILD_SHA: &str = match option_env!("AIONFORGE_BUILD_SHA") {
    Some(sha) => sha,
    None => "unknown",
};
/// `clean` or `dirty` (tracked content vs the built commit), or `unknown` off a checkout.
const BUILD_STATUS: &str = match option_env!("AIONFORGE_BUILD_STATUS") {
    Some(status) => status,
    None => "unknown",
};

/// Parameters for the `server_status` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ServerStatusToolParams {
    /// Include tool class lists and operational hints.
    #[schemars(description = "Include tool class lists and operational hints.")]
    pub verbose: Option<bool>,
}

#[derive(Serialize)]
struct ServerStatusStructured {
    schema: &'static str,
    version: &'static str,
    build: ServerStatusBuild,
    surface: ServerStatusSurface,
    transports: Vec<&'static str>,
    sampling: bool,
    recall_wrapper: &'static str,
    counts: ServerStatusCounts,
    auth: ServerStatusAuth,
    telemetry: ServerStatusTelemetry,
    resources: Vec<&'static str>,
}

#[derive(Serialize)]
struct ServerStatusBuild {
    sha: &'static str,
    profile: &'static str,
}

#[derive(Serialize)]
struct ServerStatusSurface {
    tools: usize,
    resources: usize,
    prompts: usize,
    read_like_tools: Vec<&'static str>,
    mutating_tools: Vec<&'static str>,
}

#[derive(Serialize)]
struct ServerStatusCounts {
    memories: u64,
    work_items: u64,
    kinds: ServerStatusKindCounts,
    work_statuses: ServerStatusWorkCounts,
}

#[derive(Serialize)]
struct ServerStatusKindCounts {
    episodes: u64,
    facts: u64,
    entities: u64,
    notes: u64,
    skills: u64,
    bad_patterns: u64,
}

#[derive(Serialize)]
struct ServerStatusWorkCounts {
    todo: u64,
    in_progress: u64,
    blocked: u64,
    done: u64,
    dropped: u64,
}

#[derive(Serialize)]
struct ServerStatusAuth {
    enabled: bool,
    issuers: Vec<String>,
}

#[derive(Serialize)]
struct ServerStatusTelemetry {
    memory_traffic: ServerStatusMemoryTraffic,
}

#[derive(Serialize)]
struct ServerStatusMemoryTraffic {
    bytes_in_total: u64,
    bytes_out_total: u64,
    estimated_tokens_in_total: u64,
    estimated_tokens_out_total: u64,
    token_estimate_divisor: u64,
    token_estimate_kind: &'static str,
}

/// The OAuth resource-server posture `server_status` reports — **posture only, never a secret**.
///
/// Carries the master switch and the count/origins of the trusted issuers. It deliberately holds
/// **no** JWKS, key, token, claim, or audience value: an operator can see *that* auth is on and
/// *which* issuers are trusted (by origin), but nothing cryptographic and no resource audience.
/// The default ([`AuthPosture::disabled`]) is auth-off with no issuers, the same posture a
/// default-off deployment reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthPosture {
    /// Whether the OAuth resource-server posture is enabled.
    pub enabled: bool,
    /// The trusted issuer origins (non-secret URLs), in config order. Empty when auth is disabled.
    pub issuer_origins: Vec<String>,
}

impl AuthPosture {
    /// The auth-disabled posture: the master switch off and no trusted issuers.
    #[must_use]
    pub fn disabled() -> Self {
        Self::default()
    }

    /// The auth-enabled posture with the given trusted issuer origins (non-secret URLs).
    #[must_use]
    pub fn enabled(issuer_origins: Vec<String>) -> Self {
        Self {
            enabled: true,
            issuer_origins,
        }
    }
}

/// Render compact MCP server status.
///
/// `counts` is the live, engine-native memory census (global operator telemetry): its
/// `total()` rides the base `[server]` line as `memories=N`, and the per-kind breakdown is
/// emitted only under verbose. `work_counts` is the separate work-item census: its `total()`
/// rides the base line as `work_items=N` (never merged into `memories=` — work items are not
/// memories), and the per-status breakdown rides a verbose `work_statuses=` line.
#[must_use]
pub fn server_status_tool(
    resource_count: usize,
    counts: MemoryCounts,
    work_counts: WorkCounts,
    params: ServerStatusToolParams,
    auth: &AuthPosture,
) -> String {
    server_status_tool_output(resource_count, counts, work_counts, params, auth).text
}

/// Render compact status text and the console-facing structured status DTO.
#[must_use]
pub(crate) fn server_status_tool_output(
    resource_count: usize,
    counts: MemoryCounts,
    work_counts: WorkCounts,
    params: ServerStatusToolParams,
    auth: &AuthPosture,
) -> StructuredToolOutput {
    server_status_tool_output_with_traffic(
        resource_count,
        counts,
        work_counts,
        params,
        auth,
        crate::traffic::snapshot(),
    )
}

fn server_status_tool_output_with_traffic(
    resource_count: usize,
    counts: MemoryCounts,
    work_counts: WorkCounts,
    params: ServerStatusToolParams,
    auth: &AuthPosture,
    traffic: TrafficSnapshot,
) -> StructuredToolOutput {
    let mut out = format!(
        "[server] version={} build_sha={} build={} tools={} resources={} prompts={} transports={} sampling=false recall_wrapper=recalled-memory-context mutating_tools={} memories={} work_items={} auth_enabled={} auth_issuers={}",
        env!("CARGO_PKG_VERSION"),
        BUILD_SHA,
        BUILD_STATUS,
        surface::tool_count(),
        resource_count,
        surface::PROMPT_COUNT,
        surface::TRANSPORTS_COMPACT,
        surface::tool_count_by_class(ToolClass::Mutating),
        counts.total(),
        work_counts.total(),
        auth.enabled,
        auth.issuer_origins.len(),
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
        out.push_str(&format!(
            "work_statuses=todo={} in_progress={} blocked={} done={} dropped={}",
            work_counts.todo,
            work_counts.in_progress,
            work_counts.blocked,
            work_counts.done,
            work_counts.dropped,
        ));
        out.push('\n');
        out.push_str(&format!(
            "memory_traffic=bytes_in_total={} bytes_out_total={} estimated_tokens_in_total={} estimated_tokens_out_total={} token_estimate=coarse_bytes_divisor/{}",
            traffic.bytes_in_total,
            traffic.bytes_out_total,
            traffic.estimated_tokens_in_total(),
            traffic.estimated_tokens_out_total(),
            TOKEN_ESTIMATE_BYTES_PER_TOKEN,
        ));
        // The trusted issuer ORIGINS (never JWKS, keys, the resource audience, or any token/claim)
        // are listed only when auth is enabled and at least one issuer is configured. This is the
        // posture an operator needs to confirm WHICH issuers are trusted, with nothing secret.
        if auth.enabled && !auth.issuer_origins.is_empty() {
            out.push('\n');
            out.push_str("auth_issuer_origins=");
            out.push_str(&auth.issuer_origins.join(","));
        }
        out.push('\n');
        out.push_str(
            "policy=allow_read_like_ask_mutations resources=aionforge://manifest/tools.json,aionforge://guide/mcp-surface,aionforge://policy/tool-approval",
        );
    }
    let structured = ServerStatusStructured {
        schema: "aionforge.server_status.v1",
        version: env!("CARGO_PKG_VERSION"),
        build: ServerStatusBuild {
            sha: BUILD_SHA,
            profile: BUILD_STATUS,
        },
        surface: ServerStatusSurface {
            tools: surface::tool_count(),
            resources: resource_count,
            prompts: surface::PROMPT_COUNT,
            read_like_tools: surface::tool_names_by_class(ToolClass::ReadLike),
            mutating_tools: surface::tool_names_by_class(ToolClass::Mutating),
        },
        transports: surface::TRANSPORTS.to_vec(),
        sampling: false,
        recall_wrapper: "recalled-memory-context",
        counts: ServerStatusCounts {
            memories: counts.total(),
            work_items: work_counts.total(),
            kinds: ServerStatusKindCounts {
                episodes: counts.episodes,
                facts: counts.facts,
                entities: counts.entities,
                notes: counts.notes,
                skills: counts.skills,
                bad_patterns: counts.bad_patterns,
            },
            work_statuses: ServerStatusWorkCounts {
                todo: work_counts.todo,
                in_progress: work_counts.in_progress,
                blocked: work_counts.blocked,
                done: work_counts.done,
                dropped: work_counts.dropped,
            },
        },
        auth: ServerStatusAuth {
            enabled: auth.enabled,
            issuers: auth.issuer_origins.clone(),
        },
        telemetry: ServerStatusTelemetry {
            memory_traffic: ServerStatusMemoryTraffic {
                bytes_in_total: traffic.bytes_in_total,
                bytes_out_total: traffic.bytes_out_total,
                estimated_tokens_in_total: traffic.estimated_tokens_in_total(),
                estimated_tokens_out_total: traffic.estimated_tokens_out_total(),
                token_estimate_divisor: TOKEN_ESTIMATE_BYTES_PER_TOKEN,
                token_estimate_kind: "coarse_bytes_divisor",
            },
        },
        resources: vec![
            TOOL_MANIFEST_RESOURCE_URI,
            MCP_SURFACE_GUIDE_RESOURCE_URI,
            TOOL_APPROVAL_POLICY_RESOURCE_URI,
        ],
    };
    StructuredToolOutput::new(out, structured)
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

    fn sample_work_counts() -> WorkCounts {
        WorkCounts {
            todo: 2,
            in_progress: 1,
            blocked: 0,
            done: 1,
            dropped: 0,
        }
    }

    fn sample_traffic() -> TrafficSnapshot {
        TrafficSnapshot {
            bytes_in_total: 120,
            bytes_out_total: 44,
        }
    }

    #[test]
    fn compact_status_reports_counts_and_posture() {
        let out = server_status_tool(
            8,
            sample_counts(),
            sample_work_counts(),
            ServerStatusToolParams { verbose: None },
            &AuthPosture::disabled(),
        );
        assert!(out.starts_with("[server] "), "{out}");
        assert!(out.contains("tools=18"), "{out}");
        assert!(out.contains("resources=8"), "{out}");
        assert!(out.contains("sampling=false"), "{out}");
        assert!(out.contains("memories=6"), "{out}");
        // The work-item census rides its own field, never merged into memories=.
        assert!(out.contains("work_items=4"), "{out}");
        // Build provenance rides the base line: the SHA field and a clean/dirty/unknown
        // verdict are always present (the value is whatever the build baked in).
        assert!(out.contains("build_sha="), "{out}");
        assert!(out.contains("build="), "{out}");
        assert!(
            ["build=clean", "build=dirty", "build=unknown"]
                .iter()
                .any(|status| out.contains(status)),
            "{out}"
        );
        // Default-off posture: the base line reports auth disabled with zero issuers.
        assert!(out.contains("auth_enabled=false"), "{out}");
        assert!(out.contains("auth_issuers=0"), "{out}");
        // ...and never lists origins (there are none, and the verbose origins line is absent).
        assert!(!out.contains("auth_issuer_origins="), "{out}");
    }

    #[test]
    fn enabled_auth_posture_reports_the_count_and_origins_but_no_secret() {
        let auth = AuthPosture::enabled(vec![
            "https://dev-7ppqf0duhy7etaet.us.auth0.com/".to_string(),
            "https://login.microsoftonline.com/tenant/v2.0".to_string(),
        ]);
        let out = server_status_tool(
            8,
            sample_counts(),
            sample_work_counts(),
            ServerStatusToolParams {
                verbose: Some(true),
            },
            &auth,
        );
        // Base line: enabled with the issuer count.
        assert!(out.contains("auth_enabled=true"), "{out}");
        assert!(out.contains("auth_issuers=2"), "{out}");
        // Verbose: the issuer ORIGINS are listed (non-secret URLs), comma-joined.
        assert!(
            out.contains(
                "auth_issuer_origins=https://dev-7ppqf0duhy7etaet.us.auth0.com/,\
                 https://login.microsoftonline.com/tenant/v2.0"
            ),
            "{out}"
        );
        // Never the resource audience, JWKS, keys, or any token/claim material.
        assert!(!out.contains("memory.aionforgelabs.com"), "{out}");
        assert!(!out.to_lowercase().contains("jwks"), "{out}");
        assert!(!out.to_lowercase().contains("bearer"), "{out}");
    }

    #[test]
    fn verbose_status_lists_tool_classes() {
        let out = server_status_tool_output_with_traffic(
            8,
            sample_counts(),
            sample_work_counts(),
            ServerStatusToolParams {
                verbose: Some(true),
            },
            &AuthPosture::disabled(),
            sample_traffic(),
        )
        .text;
        // The full rosters, in TOOLS order — the work tools append after the existing ones.
        assert!(
            out.contains(
                "read_like_tools=server_status,search,read_memory,session_manifest,consolidation_status,audit_history,work_tree,work_query"
            ),
            "{out}"
        );
        assert!(
            out.contains(
                "mutating_tools=capture,batch_capture,consolidate,forget,unforget,pin,unpin,work_create,work_advance,work_link"
            ),
            "{out}"
        );
        assert!(out.contains("aionforge://policy/tool-approval"));
        // The per-kind breakdown line is exact.
        let kinds_line = "kinds=episodes=3 facts=2 entities=1 notes=0 skills=0 bad_patterns=0";
        assert!(out.contains(kinds_line), "{out}");
        // The per-status work breakdown line is exact, and sits just after kinds.
        let work_line = "work_statuses=todo=2 in_progress=1 blocked=0 done=1 dropped=0";
        assert!(out.contains(work_line), "{out}");
        let traffic_line = "memory_traffic=bytes_in_total=120 bytes_out_total=44 \
                            estimated_tokens_in_total=30 estimated_tokens_out_total=11 \
                            token_estimate=coarse_bytes_divisor/4";
        assert!(out.contains(traffic_line), "{out}");
        // Ordering: mutating roster < kinds < work_statuses < policy. Anchor to the verbose
        // roster specifically — the base [server] line also contains "mutating_tools=".
        let mutating_at = out
            .find("mutating_tools=capture,batch_capture,consolidate,forget,unforget,pin,unpin,work_create,work_advance,work_link")
            .expect("verbose output has a mutating_tools roster line");
        let kinds_at = out
            .find(kinds_line)
            .expect("verbose output has a kinds line");
        let work_at = out
            .find(work_line)
            .expect("verbose output has a work_statuses line");
        let traffic_at = out
            .find(traffic_line)
            .expect("verbose output has a memory_traffic line");
        let policy_at = out
            .find("policy=allow_read_like_ask_mutations")
            .expect("verbose output has a policy line");
        assert!(
            mutating_at < kinds_at
                && kinds_at < work_at
                && work_at < traffic_at
                && traffic_at < policy_at,
            "memory_traffic line must fall between work_statuses and policy: {out}"
        );
    }

    #[test]
    fn structured_status_reports_memory_traffic_rollup() {
        let output = server_status_tool_output_with_traffic(
            8,
            sample_counts(),
            sample_work_counts(),
            ServerStatusToolParams {
                verbose: Some(true),
            },
            &AuthPosture::disabled(),
            sample_traffic(),
        );
        let traffic = output
            .structured
            .get("telemetry")
            .and_then(|telemetry| telemetry.get("memory_traffic"))
            .expect("structured telemetry has memory_traffic");

        assert_eq!(
            traffic
                .get("bytes_in_total")
                .and_then(serde_json::Value::as_u64),
            Some(120),
            "{traffic}"
        );
        assert_eq!(
            traffic
                .get("bytes_out_total")
                .and_then(serde_json::Value::as_u64),
            Some(44),
            "{traffic}"
        );
        assert_eq!(
            traffic
                .get("estimated_tokens_in_total")
                .and_then(serde_json::Value::as_u64),
            Some(30),
            "{traffic}"
        );
        assert_eq!(
            traffic
                .get("estimated_tokens_out_total")
                .and_then(serde_json::Value::as_u64),
            Some(11),
            "{traffic}"
        );
        assert_eq!(
            traffic
                .get("token_estimate_kind")
                .and_then(serde_json::Value::as_str),
            Some("coarse_bytes_divisor"),
            "{traffic}"
        );
        assert_eq!(
            traffic
                .get("token_estimate_divisor")
                .and_then(serde_json::Value::as_u64),
            Some(4),
            "{traffic}"
        );
    }
}
