//! The capture and search tool logic, free of the MCP transport so it can be tested
//! directly (04 §1, 03 §6).
//!
//! Output is compact by default — a one-line receipt for a capture, and a one-line
//! summary plus one short line per memory for a search — to keep an agent's context
//! small; `verbose` opts into per-memory detail. The search rendering is delegated to
//! [`RecallBundle::render_compact`](aionforge_engine::RecallBundle::render_compact) so the
//! recall security contract (the `recalled-memory-context` wrapper and `tag_escape` on
//! every snippet, 07 §4) is applied in one place and never re-derived here. Captures
//! arrive untrusted, so they are confined to the writer's private namespace, and a
//! search is authorized against the caller-supplied viewer namespace plus the teams the
//! host asserts for that reader.

use aionforge_domain::contracts::Embedder;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::Role;
use aionforge_domain::time::Timestamp;
use aionforge_engine::{
    CaptureReceipt, CaptureRequest, CaptureVerdict, EmbeddingOutcome, Memory, Principal,
    RecallQuery, WriterContext,
};
use schemars::JsonSchema;
use serde::Deserialize;

/// The default number of hits a search returns when the caller does not say.
const DEFAULT_LIMIT: usize = 10;
/// The most hits a single search will return, so a response stays small.
const MAX_LIMIT: usize = 100;

/// Parameters for the `capture` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CaptureToolParams {
    /// The raw event content to remember.
    #[schemars(description = "The raw event content to remember.")]
    pub content: String,
    /// The authoring agent's id (a UUID). The memory is private to this agent.
    #[schemars(
        description = "The authoring agent's id (a UUID). The memory is private to this agent."
    )]
    pub agent_id: String,
    /// The producing role: user, assistant, tool, system, or event (default user).
    #[schemars(
        description = "Producing role: user, assistant, tool, system, or event (default user)."
    )]
    pub role: Option<String>,
    /// The owning session id (a UUID), if any.
    #[schemars(description = "The owning session id (a UUID), if any.")]
    pub session_id: Option<String>,
    /// Writer trust in [0, 1] (default 0.5).
    #[schemars(description = "Writer trust in [0, 1] (default 0.5).")]
    pub trust: Option<f64>,
    /// The writer model family, recorded for provenance.
    #[schemars(description = "The writer model family, recorded for provenance.")]
    pub model_family: Option<String>,
    /// The event time as RFC3339, for backfilling a past event; defaults to capture time.
    #[schemars(
        description = "Event time as RFC3339 (e.g. 2026-06-07T12:00:00Z); defaults to capture time."
    )]
    pub captured_at: Option<String>,
}

/// Parameters for the `search` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchToolParams {
    /// The natural-language query.
    #[schemars(description = "The natural-language query.")]
    pub query: String,
    /// The reading agent's namespace, `agent:<id>`. The recall is scoped to this agent's
    /// visible set: the global space, its own private namespace, and any teams the host
    /// asserts for it (see `teams`).
    #[schemars(
        description = "The reading agent's namespace, agent:<id>. Recall is scoped to its visible set."
    )]
    pub viewer: String,
    /// The teams the host asserts this reader belongs to. Recall widens to each team's shared
    /// namespace; omit (or leave empty) for a reader that sees only the global space and its own
    /// private namespace. Host-asserted: the calling host is the team-membership authority (06 §1).
    #[serde(default)]
    #[schemars(
        description = "Teams the host asserts this reader belongs to; recall widens to each team's shared namespace. Optional."
    )]
    pub teams: Vec<String>,
    /// The maximum number of hits to return (default 10, max 100).
    #[schemars(description = "Maximum hits to return (default 10, max 100).")]
    pub limit: Option<usize>,
    /// Include per-hit detail (namespace, trust, signal contributions).
    #[schemars(description = "Include per-hit detail (namespace, trust, signal contributions).")]
    pub verbose: Option<bool>,
}

/// Run the `capture` tool: stamp the event with `now`, capture it, and return a
/// compact receipt line. Errors are returned as `ERR_*` strings for the MCP client.
///
/// # Errors
/// Returns a structured `ERR_*` message string on a bad parameter or a capture failure.
pub async fn capture_tool<E: Embedder>(
    memory: &Memory<E>,
    params: CaptureToolParams,
    now: &Timestamp,
) -> Result<String, String> {
    let agent_id = Id::parse(&params.agent_id)
        .map_err(|_| "ERR_INVALID_AGENT_ID: agent_id must be a UUID".to_string())?;
    let role = parse_role(params.role.as_deref())?;
    let session_id = params
        .session_id
        .as_deref()
        .map(Id::parse)
        .transpose()
        .map_err(|_| "ERR_INVALID_SESSION_ID: session_id must be a UUID".to_string())?;
    let captured_at = match params.captured_at.as_deref() {
        Some(raw) => parse_captured_at(raw)?,
        None => now.clone(),
    };

    let request = CaptureRequest {
        content: params.content,
        role,
        agent_id,
        // An MCP capture is structurally untrusted (`trusted: false` below), so the write is
        // confined to the writer's own private namespace regardless of team membership (04 §1,
        // 06 §1). A team-targeted write is unreachable from this surface, so the principal's
        // teams are intentionally empty here — they could not change the write target.
        teams: Vec::new(),
        session_id,
        captured_at,
        writer: WriterContext {
            model_family: params
                .model_family
                .unwrap_or_else(|| "mcp-client".to_string()),
            model_version: None,
            transport: Some("mcp".to_string()),
            request_id: None,
            trust: params.trust.unwrap_or(0.5),
            // MCP captures are unsigned; signed-write deployments reject them until the MCP
            // transport carries a host signature (out of scope for M4.T03).
            signed: None,
        },
        // MCP captures are untrusted, so the episode is confined to the writer's
        // private namespace (04 §1, 06 §1).
        trusted: false,
        namespace: None,
    };

    let receipt = memory
        .capture(request)
        .await
        .map_err(|error| format!("ERR_CAPTURE: {error}"))?;
    Ok(format_receipt(&receipt))
}

/// Run the `search` tool: recall under the viewer's authorization and render a
/// compact (or verbose) result. Errors are returned as `ERR_*` strings.
///
/// # Errors
/// Returns a structured `ERR_*` message string on a bad parameter or a search failure.
pub async fn search_tool<E: Embedder>(
    memory: &Memory<E>,
    params: SearchToolParams,
) -> Result<String, String> {
    // A reader is an agent: recall scopes to the global space, the reader's own private
    // namespace, and the teams the host asserts. A non-agent viewer has no reader identity,
    // so it is rejected rather than silently widened.
    let viewer: Namespace = params
        .viewer
        .parse()
        .map_err(|_| "ERR_INVALID_VIEWER: viewer must be agent:<id>".to_string())?;
    let Namespace::Agent(agent_id) = viewer else {
        return Err("ERR_INVALID_VIEWER: a reader must be an agent (agent:<id>)".to_string());
    };
    let agent = Id::parse(&agent_id)
        .map_err(|_| "ERR_INVALID_VIEWER: viewer agent id must be a UUID".to_string())?;
    // The host asserts the reader's team membership (the caller-asserted trust boundary, 06 §1);
    // those teams widen the visible set to each team's shared namespace. With no teams the reader
    // is scoped to the global space and its own private namespace. `Principal::new` drops any
    // empty team name.
    let principal = Principal::new(agent, params.teams);
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let verbose = params.verbose.unwrap_or(false);

    let bundle = memory
        .search(RecallQuery::new(params.query, principal, limit))
        .await
        .map_err(|error| format!("ERR_SEARCH: {error}"))?;
    // The compact rendering lives next to the full rendered view in the retrieval crate
    // so both share one `tag_escape` and the same third-party-data wrapper; the MCP
    // surface never re-derives recall text and so cannot drop the security tagging (07 §4).
    Ok(bundle.render_compact(verbose))
}

/// Parse the optional role string, defaulting to `user`.
fn parse_role(role: Option<&str>) -> Result<Role, String> {
    match role.unwrap_or("user") {
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        "tool" => Ok(Role::Tool),
        "system" => Ok(Role::System),
        "event" => Ok(Role::Event),
        other => Err(format!(
            "ERR_INVALID_ROLE: unknown role '{other}' (use user|assistant|tool|system|event)"
        )),
    }
}

/// Parse a caller-supplied RFC3339 event time into the canonical timestamp, normalized to
/// UTC. The host boundary owns the wall clock (the handler injects `now`); a caller may
/// override only the *event* time here — to backfill a past event — never read an ambient
/// clock. An unparseable value is a typed `ERR_*` rather than a silent fall-back to now.
fn parse_captured_at(raw: &str) -> Result<Timestamp, String> {
    let instant: jiff::Timestamp = raw.parse().map_err(|_| {
        "ERR_INVALID_CAPTURED_AT: captured_at must be an RFC3339 timestamp".to_string()
    })?;
    Ok(instant.to_zoned(jiff::tz::TimeZone::UTC))
}

/// A one-line capture receipt.
fn format_receipt(receipt: &CaptureReceipt) -> String {
    let verdict = match &receipt.verdict {
        CaptureVerdict::New => "new".to_string(),
        CaptureVerdict::ExactDuplicate => "exact_duplicate".to_string(),
        CaptureVerdict::NearDuplicate { distance, .. } => format!("near_duplicate({distance:.3})"),
    };
    let embedding = match &receipt.embedding {
        EmbeddingOutcome::Embedded => "embedded",
        EmbeddingOutcome::Skipped(_) => "skipped",
        EmbeddingOutcome::NotRequested => "not_requested",
    };
    format!(
        "[capture] {id} verdict={verdict} redactions={redactions} flags={flags} emb={embedding} ns={ns}",
        id = receipt.episode_id,
        redactions = receipt.redactions.len(),
        flags = receipt.injection_flags.len(),
        ns = receipt.namespace,
    )
}

#[cfg(test)]
mod tests {
    use super::parse_captured_at;

    #[test]
    fn parses_rfc3339_with_zulu_and_offset_to_the_same_instant() {
        // "Z" and an explicit offset for the same instant must normalize identically.
        let zulu = parse_captured_at("2026-06-07T12:00:00Z").expect("zulu parses");
        let offset = parse_captured_at("2026-06-07T07:00:00-05:00").expect("offset parses");
        assert_eq!(zulu.timestamp(), offset.timestamp());
        // Normalized to UTC regardless of the input offset.
        assert_eq!(zulu.time_zone(), &jiff::tz::TimeZone::UTC);
    }

    #[test]
    fn rejects_a_non_timestamp_with_a_typed_error() {
        let err = parse_captured_at("yesterday").expect_err("must reject");
        assert!(
            err.starts_with("ERR_INVALID_CAPTURED_AT"),
            "typed error: {err}"
        );
        // A bare date is not a full RFC3339 instant.
        assert!(parse_captured_at("2026-06-07").is_err());
    }
}
