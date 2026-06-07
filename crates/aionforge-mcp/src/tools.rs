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
//! search is authorized against the caller-supplied viewer namespace.

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
    /// The authoring agent's id (a ULID). The memory is private to this agent.
    #[schemars(
        description = "The authoring agent's id (a ULID). The memory is private to this agent."
    )]
    pub agent_id: String,
    /// The producing role: user, assistant, tool, system, or event (default user).
    #[schemars(
        description = "Producing role: user, assistant, tool, system, or event (default user)."
    )]
    pub role: Option<String>,
    /// The owning session id (a ULID), if any.
    #[schemars(description = "The owning session id (a ULID), if any.")]
    pub session_id: Option<String>,
    /// Writer trust in [0, 1] (default 0.5).
    #[schemars(description = "Writer trust in [0, 1] (default 0.5).")]
    pub trust: Option<f64>,
    /// The writer model family, recorded for provenance.
    #[schemars(description = "The writer model family, recorded for provenance.")]
    pub model_family: Option<String>,
}

/// Parameters for the `search` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchToolParams {
    /// The natural-language query.
    #[schemars(description = "The natural-language query.")]
    pub query: String,
    /// The viewer namespace authorization is applied against: `agent:<id>`,
    /// `team:<id>`, `global`, or `system`.
    #[schemars(
        description = "Viewer namespace for authorization: agent:<id>, team:<id>, global, or system."
    )]
    pub viewer: String,
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
        .map_err(|_| "ERR_INVALID_AGENT_ID: agent_id must be a ULID".to_string())?;
    let role = parse_role(params.role.as_deref())?;
    let session_id = params
        .session_id
        .as_deref()
        .map(Id::parse)
        .transpose()
        .map_err(|_| "ERR_INVALID_SESSION_ID: session_id must be a ULID".to_string())?;

    let request = CaptureRequest {
        content: params.content,
        role,
        agent_id,
        session_id,
        captured_at: now.clone(),
        writer: WriterContext {
            model_family: params
                .model_family
                .unwrap_or_else(|| "mcp-client".to_string()),
            model_version: None,
            transport: Some("mcp".to_string()),
            request_id: None,
            trust: params.trust.unwrap_or(0.5),
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
    let viewer: Namespace = params.viewer.parse().map_err(|_| {
        "ERR_INVALID_VIEWER: viewer must be agent:<id>, team:<id>, global, or system".to_string()
    })?;
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let verbose = params.verbose.unwrap_or(false);

    let bundle = memory
        .search(RecallQuery::new(params.query, viewer, limit))
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
