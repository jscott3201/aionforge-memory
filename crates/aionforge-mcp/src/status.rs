//! Compact MCP server status reporting.

use aionforge_engine::MemoryCounts;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::surface::{self, ToolClass};

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
/// `total()` rides the base `[server]` line as `memories=N`, and the per-kind breakdown
/// is emitted only under verbose.
#[must_use]
pub fn server_status_tool(
    resource_count: usize,
    counts: MemoryCounts,
    params: ServerStatusToolParams,
    auth: &AuthPosture,
) -> String {
    let mut out = format!(
        "[server] version={} build_sha={} build={} tools={} resources={} prompts={} transports={} sampling=false recall_wrapper=recalled-memory-context mutating_tools={} memories={} auth_enabled={} auth_issuers={}",
        env!("CARGO_PKG_VERSION"),
        BUILD_SHA,
        BUILD_STATUS,
        surface::tool_count(),
        resource_count,
        surface::PROMPT_COUNT,
        surface::TRANSPORTS_COMPACT,
        surface::tool_count_by_class(ToolClass::Mutating),
        counts.total(),
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
        let out = server_status_tool(
            8,
            sample_counts(),
            ServerStatusToolParams { verbose: None },
            &AuthPosture::disabled(),
        );
        assert!(out.starts_with("[server] "), "{out}");
        assert!(out.contains("tools=13"), "{out}");
        assert!(out.contains("resources=8"), "{out}");
        assert!(out.contains("sampling=false"), "{out}");
        assert!(out.contains("memories=6"), "{out}");
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
        let out = server_status_tool(
            8,
            sample_counts(),
            ServerStatusToolParams {
                verbose: Some(true),
            },
            &AuthPosture::disabled(),
        );
        assert!(out.contains("read_like_tools=server_status,search,read_memory,session_manifest"));
        assert!(out.contains(
            "mutating_tools=capture,batch_capture,consolidate,forget,unforget,pin,unpin"
        ));
        assert!(out.contains("aionforge://policy/tool-approval"));
        // The per-kind breakdown line is exact.
        let kinds_line = "kinds=episodes=3 facts=2 entities=1 notes=0 skills=0 bad_patterns=0";
        assert!(out.contains(kinds_line), "{out}");
        // ...and it sits between the mutating_tools roster line and the policy line.
        // Anchor to the verbose roster specifically — the base [server] line also
        // contains "mutating_tools=", so a plain find() would match that one instead.
        let mutating_at = out
            .find("mutating_tools=capture,batch_capture,consolidate,forget,unforget,pin,unpin")
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
