//! Text and structured output builders for lifecycle read tools.

use aionforge_domain::nodes::forensic::AuditKind;
use aionforge_engine::{AuditCursor, AuditPage, AuditRecord, AuditVerification, ConsolidationLag};
use serde::Serialize;

use crate::structured::StructuredToolOutput;

const PAYLOAD_PREVIEW_CHARS: usize = 240;

#[derive(Serialize)]
struct ConsolidationStatusStructured {
    schema: &'static str,
    pending: u64,
    failed: u64,
    oldest_pending_age_s: u64,
    generation: u64,
    state: &'static str,
}

#[derive(Serialize)]
struct AuditHistoryStructured {
    schema: &'static str,
    subject: String,
    kind: String,
    count: usize,
    next: Option<AuditCursorStructured>,
    records: Vec<AuditRecordStructured>,
}

#[derive(Serialize)]
struct AuditCursorStructured {
    occurred_at: String,
    id: String,
}

#[derive(Serialize)]
struct AuditRecordStructured {
    id: String,
    subject_id: String,
    kind: String,
    occurred_at: String,
    actor: String,
    namespace: String,
    verification: &'static str,
    payload_preview: Option<String>,
}

pub(crate) fn consolidation_status(lag: &ConsolidationLag, verbose: bool) -> StructuredToolOutput {
    let state = consolidation_state(lag.episodes_pending, lag.episodes_failed);
    let mut out = format!(
        "[consolidation] pending={} failed={} oldest_pending_age_s={} generation={}",
        lag.episodes_pending,
        lag.episodes_failed,
        lag.oldest_pending_lag.as_secs(),
        lag.generation
    );
    if verbose {
        out.push_str(" state=");
        out.push_str(state);
    }
    StructuredToolOutput::new(
        out,
        ConsolidationStatusStructured {
            schema: "aionforge.consolidation_status.v1",
            pending: lag.episodes_pending,
            failed: lag.episodes_failed,
            oldest_pending_age_s: lag.oldest_pending_lag.as_secs(),
            generation: lag.generation,
            state,
        },
    )
}

pub(crate) fn audit_history(
    subject: String,
    kind: String,
    include_subject: bool,
    page: &AuditPage,
    verbose: bool,
) -> StructuredToolOutput {
    StructuredToolOutput::new(
        render_audit_page(&subject, &kind, include_subject, page, verbose),
        AuditHistoryStructured {
            schema: "aionforge.audit_history.v1",
            subject,
            kind,
            count: page.records.len(),
            next: page.next.as_ref().map(structured_audit_cursor),
            records: page
                .records
                .iter()
                .map(|record| structured_audit_record(record, verbose))
                .collect(),
        },
    )
}

fn consolidation_state(pending: u64, failed: u64) -> &'static str {
    if pending == 0 && failed == 0 {
        "idle"
    } else if failed > 0 {
        "attention_required"
    } else {
        "backlog_pending"
    }
}

fn structured_audit_cursor(cursor: &AuditCursor) -> AuditCursorStructured {
    AuditCursorStructured {
        occurred_at: cursor.occurred_at.to_string(),
        id: cursor.id.to_string(),
    }
}

fn structured_audit_record(record: &AuditRecord, verbose: bool) -> AuditRecordStructured {
    let event = &record.event;
    AuditRecordStructured {
        id: event.identity.id.to_string(),
        subject_id: event.subject_id.to_string(),
        kind: audit_kind_name(event.kind),
        occurred_at: event.occurred_at.to_string(),
        actor: event.actor_id.to_string(),
        namespace: event.identity.namespace.to_string(),
        verification: verification_name(&record.verification),
        payload_preview: verbose.then(|| preview_json(&event.payload)),
    }
}

fn render_audit_page(
    subject: &str,
    kind: &str,
    include_subject: bool,
    page: &AuditPage,
    verbose: bool,
) -> String {
    let mut out = format!(
        "[audit] subject={} kind={} count={} next={}",
        subject,
        kind,
        page.records.len(),
        render_audit_cursor(page.next.as_ref())
    );
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
