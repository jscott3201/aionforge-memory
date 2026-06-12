//! The capture and search tool logic, free of the MCP transport so it can be tested
//! directly (04 §1, 03 §6).
//!
//! Output is compact by default — a one-line receipt for a capture, and a one-line
//! summary plus one short line per memory for a search — to keep an agent's context
//! small; `verbose` opts into per-memory detail. The search rendering is delegated to
//! [`RecallBundle::render_compact`](aionforge_engine::RecallBundle::render_compact) so the
//! recall security contract (the `recalled-memory-context` wrapper and `tag_escape` on
//! every snippet, 07 §4) is applied in one place and never re-derived here. Captures
//! default to the writer's private namespace. A host may deliberately assert team
//! membership and a `team:<name>` target to write shared memory; the capture funnel's
//! authorizer still gates the resolved namespace. Search is authorized against the
//! caller-supplied viewer namespace plus the teams the host asserts for that reader.

use aionforge_domain::contracts::Embedder;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::Role;
use aionforge_domain::time::Timestamp;
use aionforge_engine::{
    CaptureReceipt, CaptureRequest, CaptureVerdict, EmbeddingOutcome, Memory, RecallQuery,
    WriterContext,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::principal::{HostPrincipalToolParam, resolve_reader, resolve_writer};

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
    /// The authoring agent's id (a UUID). Legacy shorthand for `principal.agent_id`.
    #[serde(default)]
    #[schemars(
        description = "The authoring agent's id (a UUID). Legacy shorthand for principal.agent_id."
    )]
    pub agent_id: Option<String>,
    /// Explicit host-verified principal. OAuth-capable hosts can pass the verified
    /// token subject and teams here instead of asking the server to infer them.
    #[schemars(description = "Explicit host-verified principal. Optional.")]
    pub principal: Option<HostPrincipalToolParam>,
    /// Teams the host asserts this writer belongs to. Only used when `target_namespace`
    /// asks for a team namespace; omitted/empty keeps the capture private.
    #[serde(default)]
    #[schemars(
        description = "Teams the host asserts this writer belongs to. Required to capture into a matching team namespace."
    )]
    pub teams: Vec<String>,
    /// Optional shared write target, currently `team:<name>` or this writer's own `agent:<id>`.
    /// Omit for a private capture. The MCP server never infers this from session or content.
    #[schemars(
        description = "Optional explicit write target namespace, such as team:project-alpha. Omit for a private capture."
    )]
    pub target_namespace: Option<String>,
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
    /// The id of a live memory this capture replaces (a writer-asserted supersession
    /// hint). Recorded as evidence for consolidation; the target must be a memory this
    /// writer could write, or the capture is refused.
    #[schemars(
        description = "Id of a live memory this capture replaces; consolidation evidence, not an immediate action. Must be the writer's own memory."
    )]
    pub supersedes: Option<String>,
}

/// Parameters for the `search` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchToolParams {
    /// The natural-language query.
    #[schemars(description = "The natural-language query.")]
    pub query: String,
    /// The reading agent's namespace, `agent:<id>`. Legacy shorthand for `principal.agent_id`.
    /// The recall is scoped to this agent's
    /// visible set: the global space, its own private namespace, and any teams the host
    /// asserts for it (see `teams`).
    #[serde(default)]
    #[schemars(
        description = "The reading agent's namespace, agent:<id>. Legacy shorthand for principal.agent_id."
    )]
    pub viewer: Option<String>,
    /// Explicit host-verified principal. OAuth-capable hosts can pass the verified
    /// token subject and teams here instead of asking the server to infer them.
    #[schemars(description = "Explicit host-verified principal. Optional.")]
    pub principal: Option<HostPrincipalToolParam>,
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
    /// Include older episodes that have a live replacement claim. Defaults to true so
    /// recall preserves provenance unless the caller explicitly asks for current-only
    /// episode evidence.
    #[schemars(
        description = "Include episodes that have been superseded by a live replacement (default true)."
    )]
    pub include_superseded: Option<bool>,
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
    let (agent_id, teams) =
        resolve_writer(params.agent_id.as_deref(), params.teams, params.principal)?;
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
    let namespace = params
        .target_namespace
        .as_deref()
        .map(parse_target_namespace)
        .transpose()?;
    let supersedes = params
        .supersedes
        .as_deref()
        .map(Id::parse)
        .transpose()
        .map_err(|_| "ERR_INVALID_SUPERSEDES: supersedes must be a memory id (UUID)".to_string())?;

    let request = CaptureRequest {
        content: params.content,
        role,
        agent_id,
        // The host, not the MCP server, is the principal/team authority. Without an explicit
        // target namespace the write remains private even if teams are present; a shared write
        // requires both `target_namespace` and matching host-asserted membership, then the
        // capture funnel authorizer makes the final decision (06 §1).
        teams,
        session_id,
        captured_at,
        ingested_at: now.clone(),
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
        // A target namespace is an explicit host assertion; omitting it keeps the capture on
        // the untrusted private path. The MCP server never guesses a shared target from content
        // or session metadata.
        trusted: namespace.is_some(),
        namespace,
        supersedes,
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
/// `now` is the host boundary's wall clock, injected by the handler exactly as for a
/// capture: the substrate keeps no ambient clock, so the importance and recency
/// re-ranks exist only because this surface stamps the recall instant onto the query's
/// options (05 §2, M5.T01). The clock shapes ranking only — a recall stays read-only,
/// and the decayed importance it computes is never written back.
///
/// # Errors
/// Returns a structured `ERR_*` message string on a bad parameter or a search failure.
pub async fn search_tool<E: Embedder>(
    memory: &Memory<E>,
    params: SearchToolParams,
    now: &Timestamp,
) -> Result<String, String> {
    // A reader is an agent: recall scopes to the global space, the reader's own private
    // namespace, and the teams the host asserts. A non-agent viewer has no reader identity,
    // so it is rejected rather than silently widened.
    // The host asserts the reader's principal and team membership (the caller-asserted
    // trust boundary, 06 §1). OAuth-capable hosts may pass the verified identity in the
    // explicit `principal` object; older clients keep using `viewer` plus `teams`. If both
    // are present they must agree, so the MCP server never guesses or silently merges two
    // authority sources.
    let principal = resolve_reader(params.viewer.as_deref(), params.teams, params.principal)?;
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let verbose = params.verbose.unwrap_or(false);

    let mut query = RecallQuery::new(params.query, principal, limit);
    query.options.now = Some(now.clone());
    query.options.include_superseded = params.include_superseded.unwrap_or(true);
    let bundle = memory
        .search(query)
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

fn parse_target_namespace(raw: &str) -> Result<Namespace, String> {
    let namespace: Namespace = raw.parse().map_err(|_| {
        "ERR_INVALID_TARGET_NAMESPACE: target_namespace must be agent:<id> or team:<name>"
            .to_string()
    })?;
    match &namespace {
        Namespace::Agent(agent) => {
            Id::parse(agent).map_err(|_| {
                "ERR_INVALID_TARGET_NAMESPACE: agent namespace id must be a UUID".to_string()
            })?;
            Ok(namespace)
        }
        Namespace::Team(name) if !name.trim().is_empty() => Ok(namespace),
        Namespace::Team(_) => {
            Err("ERR_INVALID_TARGET_NAMESPACE: team namespace must not be empty".to_string())
        }
        Namespace::Global | Namespace::System => Err(
            "ERR_INVALID_TARGET_NAMESPACE: capture may target only agent or team namespaces"
                .to_string(),
        ),
    }
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
        EmbeddingOutcome::NotRequested => "not_requested",
    };
    let supersedes = match &receipt.supersedes {
        Some(target) => format!(" sup={target}"),
        None => String::new(),
    };
    format!(
        "[capture] {id} verdict={verdict} redactions={redactions} flags={flags} emb={embedding} ns={ns}{supersedes}",
        id = receipt.episode_id,
        redactions = receipt.redactions.len(),
        flags = format_injection_flags(&receipt.injection_flags),
        ns = receipt.namespace,
    )
}

fn format_injection_flags(flags: &[String]) -> String {
    if flags.is_empty() {
        return "0".to_string();
    }
    format!("{}[{}]", flags.len(), flags.join(","))
}

#[cfg(test)]
mod tests {
    use aionforge_domain::ids::Id;
    use aionforge_domain::namespace::Namespace;
    use aionforge_engine::{CaptureReceipt, CaptureVerdict, EmbeddingOutcome};

    use super::{format_injection_flags, format_receipt, parse_captured_at};

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

    #[test]
    fn capture_receipt_names_injection_flags_when_present() {
        assert_eq!(format_injection_flags(&[]), "0");
        assert_eq!(
            format_injection_flags(&[
                "ignore_or_forget_context".to_string(),
                "system_prompt".to_string(),
            ]),
            "2[ignore_or_forget_context,system_prompt]"
        );

        let receipt = CaptureReceipt {
            episode_id: Id::generate(),
            verdict: CaptureVerdict::New,
            audit_id: Some(Id::generate()),
            namespace: Namespace::Agent("0198b7d6-4d40-7000-8000-000000000001".to_string()),
            redactions: Vec::new(),
            injection_flags: vec![
                "ignore_or_forget_context".to_string(),
                "system_prompt".to_string(),
            ],
            embedding: EmbeddingOutcome::Embedded,
            supersedes: None,
        };
        let line = format_receipt(&receipt);
        assert!(
            line.contains("flags=2[ignore_or_forget_context,system_prompt]"),
            "{line}"
        );
    }
}
