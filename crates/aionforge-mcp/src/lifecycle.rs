//! Lifecycle and audit tool logic for the MCP surface.
//!
//! These helpers keep host-maintenance operations small and explicitly scoped at the
//! MCP boundary. Point forget/unforget resolve the target memory first and require it
//! to sit in a namespace writable by the supplied principal before calling the engine's maintenance
//! primitive.

use std::time::Duration;

use aionforge_domain::contracts::Embedder;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::episodic::Episode;
use aionforge_domain::nodes::forensic::AuditKind;
use aionforge_domain::nodes::procedural::{BadPattern, Skill};
use aionforge_domain::nodes::semantic::{Entity, Fact};
use aionforge_domain::time::Timestamp;
use aionforge_engine::{
    AuditCursor, AuditPage, AuditRecord, AuditVerification, Memory, PointForget, PointPin,
    PointUnforget, PointUnpin, RuleExtractor, RuleInducer, RuleSummarizer, TickReport,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::principal::{
    AuthEnabled, HostPrincipalToolParam, refuse_read_only_write, resolve_reader,
};
use crate::validated::ValidatedPrincipal;

const DEFAULT_CONSOLIDATION_MAX_TICKS: usize = 1;
const MAX_CONSOLIDATION_MAX_TICKS: usize = 5;
const DEFAULT_AUDIT_LIMIT: usize = 20;
const MAX_AUDIT_LIMIT: usize = 50;
const PAYLOAD_PREVIEW_CHARS: usize = 240;

/// The lifecycle kinds the MCP surface resolves by id for the **forgettable/pointable**
/// path — `forget`/`unforget`/`pin`/`unpin` (here) and the base of `read_memory`'s read set
/// (`inspect.rs`). Shared so read and write breadth stay in lockstep when a kind is added.
///
/// Deliberately excludes `CoreBlock`: core blocks are forgetting-exempt, so they are not a
/// valid forget/pin target. `read_memory` resolves them too, by appending `CoreBlock::LABEL`
/// to its own read set rather than widening this write-side set.
pub(crate) const MCP_MEMORY_LABELS: [&str; 6] = [
    Episode::LABEL,
    Fact::LABEL,
    Entity::LABEL,
    Note::LABEL,
    Skill::LABEL,
    BadPattern::LABEL,
];

/// Parameters for the `consolidation_status` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConsolidationStatusToolParams {
    /// Include an explanatory status hint.
    #[schemars(description = "Include an explanatory status hint.")]
    pub verbose: Option<bool>,
}

/// Parameters for the `consolidate` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConsolidationRunToolParams {
    /// Maximum foreground ticks to run (default 1, max 5).
    #[schemars(description = "Maximum foreground ticks to run (default 1, max 5).")]
    pub max_ticks: Option<usize>,
    /// Include the server-owned pass profile.
    #[schemars(description = "Include the server-owned pass profile.")]
    pub verbose: Option<bool>,
}

/// Parameters for point forget/unforget tools.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct MemoryLifecycleToolParams {
    /// The memory id to mutate.
    #[schemars(description = "The memory id to mutate.")]
    pub memory_id: String,
    /// The acting agent namespace, `agent:<id>`. The memory must be in this agent's writable set.
    #[serde(default)]
    #[schemars(
        description = "The acting agent namespace, agent:<id>. Legacy shorthand for principal.agent_id."
    )]
    pub viewer: Option<String>,
    /// Explicit host-verified principal. OAuth-capable hosts can pass the verified
    /// token subject and teams here instead of asking the server to infer them.
    #[schemars(description = "Explicit host-verified principal. Optional.")]
    pub principal: Option<HostPrincipalToolParam>,
    /// Teams the host asserts this reader belongs to.
    #[serde(default)]
    #[schemars(description = "Teams the host asserts this reader belongs to. Optional.")]
    pub teams: Vec<String>,
}

/// A keyset cursor returned by `audit_history`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AuditCursorToolParam {
    /// The `occurred_at` value returned in the prior cursor.
    #[schemars(description = "The occurred_at value returned in the prior cursor.")]
    pub occurred_at: String,
    /// The audit event id returned in the prior cursor.
    #[schemars(description = "The audit event id returned in the prior cursor.")]
    pub id: String,
}

/// Parameters for the `audit_history` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AuditHistoryToolParams {
    /// The memory or node id whose audit history should be read. Omit only when `kind`
    /// is provided to read all visible events of that kind.
    #[schemars(
        description = "The memory or node id whose audit history should be read. Omit only when kind is provided to read all visible events of that kind."
    )]
    pub subject_id: Option<String>,
    /// The reading agent namespace, `agent:<id>`.
    #[serde(default)]
    #[schemars(description = "The reading agent namespace, agent:<id>.")]
    pub viewer: Option<String>,
    /// Explicit host-verified principal. OAuth-capable hosts can pass the verified
    /// token subject and teams here instead of asking the server to infer them.
    #[schemars(description = "Explicit host-verified principal. Optional.")]
    pub principal: Option<HostPrincipalToolParam>,
    /// Teams the host asserts this reader belongs to.
    #[serde(default)]
    #[schemars(description = "Teams the host asserts this reader belongs to. Optional.")]
    pub teams: Vec<String>,
    /// Optional snake_case audit kind filter, such as `forget` or `capture`.
    #[schemars(description = "Optional snake_case audit kind filter, such as forget or capture.")]
    pub kind: Option<String>,
    /// Continuation cursor returned by a prior audit_history call.
    #[schemars(description = "Continuation cursor returned by a prior audit_history call.")]
    pub after: Option<AuditCursorToolParam>,
    /// Maximum rows to return (default 20, max 50).
    #[schemars(description = "Maximum rows to return (default 20, max 50).")]
    pub limit: Option<usize>,
    /// Include compact payload previews.
    #[schemars(description = "Include compact payload previews.")]
    pub verbose: Option<bool>,
}

struct WritableMemory {
    id: Id,
    label: String,
    namespace: Namespace,
}

/// Render the current consolidation backlog.
///
/// # Errors
/// Returns a structured `ERR_*` string if the backlog query fails.
pub fn consolidation_status_tool<E: Embedder>(
    memory: &Memory<E>,
    params: ConsolidationStatusToolParams,
    now: &Timestamp,
) -> Result<String, String> {
    let lag = memory
        .consolidation_lag(now)
        .map_err(|error| format!("ERR_CONSOLIDATION_STATUS: {error}"))?;
    let mut out = format!(
        "[consolidation] pending={} failed={} oldest_pending_age_s={} generation={}",
        lag.episodes_pending,
        lag.episodes_failed,
        duration_seconds(lag.oldest_pending_lag),
        lag.generation
    );
    if params.verbose.unwrap_or(false) {
        let state = if lag.episodes_pending == 0 && lag.episodes_failed == 0 {
            "idle"
        } else if lag.episodes_failed > 0 {
            "attention_required"
        } else {
            "backlog_pending"
        };
        out.push_str(" state=");
        out.push_str(state);
    }
    Ok(out)
}

/// Run a bounded foreground consolidation pass using server-owned deterministic rules.
///
/// # Errors
/// Returns a structured `ERR_*` string if the foreground tick fails.
pub async fn consolidate_tool<E: Embedder + 'static>(
    memory: &Memory<E>,
    params: ConsolidationRunToolParams,
) -> Result<String, String> {
    let max_ticks = params
        .max_ticks
        .unwrap_or(DEFAULT_CONSOLIDATION_MAX_TICKS)
        .clamp(1, MAX_CONSOLIDATION_MAX_TICKS);
    let mut ticks = 0usize;
    let mut total = TickReport::default();
    for _ in 0..max_ticks {
        let report = memory
            .consolidate_once(
                RuleExtractor::with_default_rules(),
                RuleSummarizer::with_default_rules(),
                RuleInducer::with_default_rules(),
                memory.consolidation_config(),
                memory.pass_config(),
            )
            .await
            .map_err(|error| format!("ERR_CONSOLIDATE: {error}"))?;
        ticks += 1;
        total.consolidated += report.consolidated;
        total.retried += report.retried;
        total.failed += report.failed;
        total.pending_after = report.pending_after;

        if report.pending_after == 0
            || report.retried > 0
            || report.failed > 0
            || (report.consolidated == 0 && report.retried == 0 && report.failed == 0)
        {
            break;
        }
    }

    let mut out = format!(
        "[consolidate] ticks={} consolidated={} retried={} failed={} pending_after={}",
        ticks, total.consolidated, total.retried, total.failed, total.pending_after
    );
    if params.verbose.unwrap_or(false) {
        out.push_str(" mode=foreground rule_set=deterministic_defaults");
    }
    Ok(out)
}

/// Soft-forget one writable memory by id.
///
/// # Errors
/// Returns a structured `ERR_*` string if parameters are invalid, the target is not
/// writable by the viewer, or the engine returns an error.
pub fn forget_tool<E: Embedder>(
    memory: &Memory<E>,
    params: MemoryLifecycleToolParams,
    now: &Timestamp,
    extension: Option<ValidatedPrincipal>,
    auth_enabled: AuthEnabled,
) -> Result<String, String> {
    let target = writable_memory(
        memory,
        &params.memory_id,
        params.viewer.as_deref(),
        params.teams,
        params.principal,
        extension,
        auth_enabled,
    )?;
    let outcome = memory
        .forget(&target.id, now)
        .map_err(|error| format!("ERR_FORGET: {error}"))?;
    Ok(format!(
        "[forget] {} kind={} ns={} outcome={}",
        target.id,
        target.label,
        target.namespace,
        point_forget_outcome(outcome)
    ))
}

/// Restore one writable soft-forgotten memory by id.
///
/// # Errors
/// Returns a structured `ERR_*` string if parameters are invalid, the target is not
/// writable by the viewer, or the engine returns an error.
pub fn unforget_tool<E: Embedder>(
    memory: &Memory<E>,
    params: MemoryLifecycleToolParams,
    now: &Timestamp,
    extension: Option<ValidatedPrincipal>,
    auth_enabled: AuthEnabled,
) -> Result<String, String> {
    let target = writable_memory(
        memory,
        &params.memory_id,
        params.viewer.as_deref(),
        params.teams,
        params.principal,
        extension,
        auth_enabled,
    )?;
    let outcome = memory
        .unforget(&target.id, now)
        .map_err(|error| format!("ERR_UNFORGET: {error}"))?;
    Ok(format!(
        "[unforget] {} kind={} ns={} outcome={}",
        target.id,
        target.label,
        target.namespace,
        point_unforget_outcome(outcome)
    ))
}

/// Pin one writable memory by id so decay and forgetting spare it (05 §2, M5.T02 rider).
///
/// Resolves and authorizes the target exactly like `forget`, then calls the always-available
/// engine pin (there is no off-switch: a pin can only spare, never doom, and read-time decay
/// honors it whether or not active forgetting is enabled). Idempotent: pinning an
/// already-pinned memory is a no-op `already_pinned` outcome, audited only on a real change.
///
/// # Errors
/// Returns a structured `ERR_*` string if parameters are invalid, the target is not
/// writable by the viewer, or the engine returns an error.
pub fn pin_tool<E: Embedder>(
    memory: &Memory<E>,
    params: MemoryLifecycleToolParams,
    now: &Timestamp,
    extension: Option<ValidatedPrincipal>,
    auth_enabled: AuthEnabled,
) -> Result<String, String> {
    let target = writable_memory(
        memory,
        &params.memory_id,
        params.viewer.as_deref(),
        params.teams,
        params.principal,
        extension,
        auth_enabled,
    )?;
    let outcome = memory
        .pin(&target.id, now)
        .map_err(|error| format!("ERR_PIN: {error}"))?;
    Ok(format!(
        "[pin] {} kind={} ns={} outcome={}",
        target.id,
        target.label,
        target.namespace,
        point_pin_outcome(outcome)
    ))
}

/// Lift the pin on one writable memory by id so decay and forgetting eligibility re-arm.
///
/// A pin is a stay, not a vault: unpinning silently re-arms decay and sweep eligibility, and
/// the memory is forgotten later only if every eligibility axis independently holds low.
/// Idempotent: unpinning a memory that is not pinned is a no-op `not_pinned` outcome.
///
/// # Errors
/// Returns a structured `ERR_*` string if parameters are invalid, the target is not
/// writable by the viewer, or the engine returns an error.
pub fn unpin_tool<E: Embedder>(
    memory: &Memory<E>,
    params: MemoryLifecycleToolParams,
    now: &Timestamp,
    extension: Option<ValidatedPrincipal>,
    auth_enabled: AuthEnabled,
) -> Result<String, String> {
    let target = writable_memory(
        memory,
        &params.memory_id,
        params.viewer.as_deref(),
        params.teams,
        params.principal,
        extension,
        auth_enabled,
    )?;
    let outcome = memory
        .unpin(&target.id, now)
        .map_err(|error| format!("ERR_UNPIN: {error}"))?;
    Ok(format!(
        "[unpin] {} kind={} ns={} outcome={}",
        target.id,
        target.label,
        target.namespace,
        point_unpin_outcome(outcome)
    ))
}

/// Read a principal-scoped audit history page for a subject.
///
/// # Errors
/// Returns a structured `ERR_*` string if parameters are invalid or the audit read fails.
pub fn audit_history_tool<E: Embedder>(
    memory: &Memory<E>,
    params: AuditHistoryToolParams,
    extension: Option<ValidatedPrincipal>,
    auth_enabled: AuthEnabled,
) -> Result<String, String> {
    let subject = params
        .subject_id
        .as_deref()
        .map(|subject| parse_id(subject.trim(), "SUBJECT_ID"))
        .transpose()?;
    let principal = resolve_reader(
        params.viewer.as_deref(),
        params.teams,
        params.principal,
        extension,
        auth_enabled,
    )?;
    let after = params.after.map(parse_audit_cursor).transpose()?;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_AUDIT_LIMIT)
        .clamp(1, MAX_AUDIT_LIMIT);
    let verbose = params.verbose.unwrap_or(false);
    let kind_filter = params
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|kind| !kind.is_empty());
    let kind = kind_filter.map(parse_audit_kind).transpose()?;

    let (scope, kind_label, page) = match (subject.as_ref(), kind) {
        (Some(subject), Some(kind)) => {
            let page = memory
                .audit_by_subject_kind(&principal, subject, kind, after.as_ref(), limit)
                .map_err(|error| format!("ERR_AUDIT_HISTORY: {error}"))?;
            (
                AuditRenderScope::Subject(subject),
                audit_kind_name(kind),
                page,
            )
        }
        (Some(subject), None) => {
            let page = memory
                .audit_history(&principal, subject, after.as_ref(), limit)
                .map_err(|error| format!("ERR_AUDIT_HISTORY: {error}"))?;
            (AuditRenderScope::Subject(subject), "all".to_string(), page)
        }
        (None, Some(kind)) => {
            let page = memory
                .audit_by_kind(&principal, kind, after.as_ref(), limit)
                .map_err(|error| format!("ERR_AUDIT_HISTORY: {error}"))?;
            (AuditRenderScope::AllVisible, audit_kind_name(kind), page)
        }
        (None, None) => {
            return Err(
                "ERR_INVALID_AUDIT_QUERY: subject_id is required unless kind is provided"
                    .to_string(),
            );
        }
    };

    Ok(render_audit_page(scope, &kind_label, &page, verbose))
}

fn duration_seconds(duration: Duration) -> u64 {
    duration.as_secs()
}

fn writable_memory<E: Embedder>(
    memory: &Memory<E>,
    raw_id: &str,
    raw_viewer: Option<&str>,
    teams: Vec<String>,
    principal: Option<HostPrincipalToolParam>,
    extension: Option<ValidatedPrincipal>,
    auth_enabled: AuthEnabled,
) -> Result<WritableMemory, String> {
    let id = parse_id(raw_id, "MEMORY_ID")?;
    // forget/unforget/pin/unpin are writes (they mutate durable memory). The point ops resolve
    // identity through the read scope (`resolve_reader`) and then namespace-authorize the write,
    // so the read-only write-guard does not flow through `resolve_writer` here — apply the *same*
    // shared guard `resolve_writer` uses, so a validated read-only/ephemeral identity may never
    // mutate durable memory and there is no second, drift-prone copy of the check.
    refuse_read_only_write(extension.as_ref(), auth_enabled)?;
    let principal = resolve_reader(raw_viewer, teams, principal, extension, auth_enabled)?;
    let candidate = memory
        .store()
        .memory_by_id(&id, &MCP_MEMORY_LABELS)
        .map_err(|error| format!("ERR_LOOKUP: {error}"))?;
    let Some(candidate) = candidate else {
        return Err("ERR_NOT_FOUND: memory_id not found or not authorized".to_string());
    };
    if memory
        .authorizer()
        .authorize_write(&principal, &candidate.identity.namespace)
        .is_err()
    {
        return Err("ERR_NOT_FOUND: memory_id not found or not authorized".to_string());
    }
    Ok(WritableMemory {
        id,
        label: candidate.label,
        namespace: candidate.identity.namespace,
    })
}

fn parse_id(raw: &str, field: &str) -> Result<Id, String> {
    Id::parse(raw).map_err(|_| format!("ERR_INVALID_{field}: {field} must be a UUID"))
}

fn parse_audit_cursor(cursor: AuditCursorToolParam) -> Result<AuditCursor, String> {
    let occurred_at = cursor
        .occurred_at
        .parse::<Timestamp>()
        .map_err(|_| "ERR_INVALID_AUDIT_CURSOR: occurred_at must be a timestamp".to_string())?;
    let id = parse_id(&cursor.id, "AUDIT_CURSOR_ID")?;
    Ok(AuditCursor { occurred_at, id })
}

fn parse_audit_kind(raw: &str) -> Result<AuditKind, String> {
    serde_json::from_value(serde_json::Value::String(raw.to_string())).map_err(|_| {
        format!("ERR_INVALID_AUDIT_KIND: unknown kind '{raw}' (use snake_case audit kind)")
    })
}

enum AuditRenderScope<'a> {
    Subject(&'a Id),
    AllVisible,
}

impl AuditRenderScope<'_> {
    fn subject_label(&self) -> String {
        match self {
            Self::Subject(subject) => subject.to_string(),
            Self::AllVisible => "*".to_string(),
        }
    }

    fn include_subject_per_record(&self) -> bool {
        matches!(self, Self::AllVisible)
    }
}

fn render_audit_page(
    scope: AuditRenderScope<'_>,
    kind_label: &str,
    page: &AuditPage,
    verbose: bool,
) -> String {
    let mut out = format!(
        "[audit] subject={} kind={} count={} next={}",
        scope.subject_label(),
        kind_label,
        page.records.len(),
        render_audit_cursor(page.next.as_ref())
    );
    let include_subject = scope.include_subject_per_record();
    for record in &page.records {
        out.push('\n');
        out.push_str(&render_audit_record(record, verbose, include_subject));
    }
    out
}

fn render_audit_record(record: &AuditRecord, verbose: bool, include_subject: bool) -> String {
    let event = &record.event;
    let mut out = format!(
        "- id={} kind={} at={} actor={} ns={} verification={}",
        event.identity.id,
        audit_kind_name(event.kind),
        event.occurred_at,
        event.actor_id,
        event.identity.namespace,
        verification_name(&record.verification)
    );
    if include_subject {
        out.push_str(" subject=");
        out.push_str(&event.subject_id.to_string());
    }
    if verbose {
        out.push_str(" payload=");
        out.push_str(&preview_json(&event.payload));
    }
    out
}

fn render_audit_cursor(cursor: Option<&AuditCursor>) -> String {
    cursor
        .map(|cursor| serde_json::to_string(cursor).expect("audit cursor serializes"))
        .unwrap_or_else(|| "none".to_string())
}

fn point_forget_outcome(outcome: PointForget) -> String {
    match outcome {
        PointForget::Forgotten => "forgotten".to_string(),
        PointForget::AlreadyForgotten => "already_forgotten".to_string(),
        PointForget::NotFound => "not_found".to_string(),
        PointForget::Protected(reason) => format!("protected({reason:?})"),
        PointForget::Disabled => "disabled reason=forgetting.enabled=false".to_string(),
    }
}

fn point_unforget_outcome(outcome: PointUnforget) -> String {
    match outcome {
        PointUnforget::Restored => "restored".to_string(),
        PointUnforget::NotForgotten => "not_forgotten".to_string(),
        PointUnforget::NotFound => "not_found".to_string(),
        PointUnforget::Protected(reason) => format!("protected({reason:?})"),
        PointUnforget::Disabled => "disabled reason=forgetting.enabled=false".to_string(),
    }
}

fn point_pin_outcome(outcome: PointPin) -> &'static str {
    match outcome {
        PointPin::Pinned => "pinned",
        PointPin::AlreadyPinned => "already_pinned",
        PointPin::NotFound => "not_found",
    }
}

fn point_unpin_outcome(outcome: PointUnpin) -> &'static str {
    match outcome {
        PointUnpin::Unpinned => "unpinned",
        PointUnpin::NotPinned => "not_pinned",
        PointUnpin::NotFound => "not_found",
    }
}

fn audit_kind_name(kind: AuditKind) -> String {
    match serde_json::to_value(kind).expect("audit kind serializes") {
        serde_json::Value::String(kind) => kind,
        _ => "unknown".to_string(),
    }
}

fn verification_name(verification: &AuditVerification) -> &'static str {
    match verification {
        AuditVerification::NotEnabled => "not_enabled",
        AuditVerification::Checked(status) => match status {
            aionforge_engine::AuditStatus::Valid => "valid",
            aionforge_engine::AuditStatus::Unsigned => "unsigned",
            aionforge_engine::AuditStatus::Downgraded => "downgraded",
            aionforge_engine::AuditStatus::Invalid => "invalid",
            aionforge_engine::AuditStatus::Untrusted => "untrusted",
        },
    }
}

fn preview_json(value: &serde_json::Value) -> String {
    let raw = serde_json::to_string(value).expect("JSON value serializes");
    truncate_chars(&raw, PAYLOAD_PREVIEW_CHARS)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut out: String = value.chars().take(max_chars).collect();
    out.push_str("...");
    out
}
