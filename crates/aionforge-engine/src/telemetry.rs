//! Low-cardinality metric emission for the engine facade.

use std::time::Duration;

use aionforge_capture::{CaptureError, CaptureReceipt, CaptureVerdict, EmbeddingOutcome};
use aionforge_consolidate::{DistillError, DistillationReport, LinkEvolveError, LinkEvolveReport};
use aionforge_forget::{CoolingSweepReport, DriftSweepReport, ForgetSweepPage};
use aionforge_retrieval::{QueryClass, RecallBundle};
use aionforge_store::StoreDoctorReport;

use crate::StartupWarning;

const NONE: &str = "none";

pub(crate) fn capture_result(result: &Result<CaptureReceipt, CaptureError>, elapsed: Duration) {
    match result {
        Ok(receipt) => capture_success(receipt, elapsed),
        Err(error) => capture_error(error, elapsed),
    }
}

fn capture_success(receipt: &CaptureReceipt, elapsed: Duration) {
    let verdict = capture_verdict_label(&receipt.verdict);
    let embedding = embedding_label(&receipt.embedding);
    ::metrics::counter!(
        "aionforge_capture_requests_total",
        "outcome" => "success",
        "verdict" => verdict,
        "embedding" => embedding,
        "error" => NONE,
    )
    .increment(1);
    ::metrics::histogram!(
        "aionforge_capture_duration_seconds",
        "outcome" => "success",
        "verdict" => verdict,
        "embedding" => embedding,
        "error" => NONE,
    )
    .record(elapsed.as_secs_f64());
    if !receipt.redactions.is_empty() {
        ::metrics::counter!("aionforge_capture_redactions_total")
            .increment(receipt.redactions.len() as u64);
    }
}

fn capture_error(error: &CaptureError, elapsed: Duration) {
    let error = capture_error_label(error);
    ::metrics::counter!(
        "aionforge_capture_requests_total",
        "outcome" => "error",
        "verdict" => NONE,
        "embedding" => NONE,
        "error" => error,
    )
    .increment(1);
    ::metrics::histogram!(
        "aionforge_capture_duration_seconds",
        "outcome" => "error",
        "verdict" => NONE,
        "embedding" => NONE,
        "error" => error,
    )
    .record(elapsed.as_secs_f64());
}

pub(crate) fn recall_result(
    result: &Result<RecallBundle, aionforge_retrieval::RetrievalError>,
    elapsed: Duration,
) {
    match result {
        Ok(bundle) => recall_success(bundle, elapsed),
        Err(error) => recall_error(retrieval_error_label(error), elapsed),
    }
}

fn recall_success(bundle: &RecallBundle, elapsed: Duration) {
    let class = query_class_label(bundle.explanation.class);
    let embedder = if bundle.explanation.embedder_available {
        "available"
    } else {
        "unavailable"
    };
    ::metrics::counter!(
        "aionforge_recall_requests_total",
        "outcome" => "success",
        "class" => class,
        "embedder" => embedder,
        "error" => NONE,
    )
    .increment(1);
    ::metrics::histogram!(
        "aionforge_recall_duration_seconds",
        "outcome" => "success",
        "class" => class,
        "embedder" => embedder,
        "error" => NONE,
    )
    .record(elapsed.as_secs_f64());
    ::metrics::histogram!("aionforge_recall_candidates_considered", "class" => class)
        .record(bundle.explanation.candidates_considered as f64);
    ::metrics::histogram!("aionforge_recall_entries_returned", "class" => class)
        .record(bundle.explanation.returned as f64);
    emit_recall_stage(class, "classify", bundle.explanation.timings_ms.classify);
    emit_recall_stage(class, "signals", bundle.explanation.timings_ms.signals);
    emit_recall_stage(class, "assemble", bundle.explanation.timings_ms.assemble);
}

pub(crate) fn recall_error(error: &'static str, elapsed: Duration) {
    ::metrics::counter!(
        "aionforge_recall_requests_total",
        "outcome" => "error",
        "class" => "unknown",
        "embedder" => "unknown",
        "error" => error,
    )
    .increment(1);
    ::metrics::histogram!(
        "aionforge_recall_duration_seconds",
        "outcome" => "error",
        "class" => "unknown",
        "embedder" => "unknown",
        "error" => error,
    )
    .record(elapsed.as_secs_f64());
}

pub(crate) fn startup_warnings(warnings: &[StartupWarning]) {
    for warning in warnings {
        let kind = match warning {
            StartupWarning::SingleFamilyDeployment { .. } => "single_family_deployment",
        };
        ::metrics::counter!("aionforge_startup_warnings_total", "kind" => kind).increment(1);
    }
}

pub(crate) fn distillation_result(
    result: &Result<DistillationReport, DistillError>,
    elapsed: Duration,
) {
    match result {
        Ok(report) => distillation_report(report, elapsed),
        Err(_) => distillation_error(elapsed),
    }
}

fn distillation_report(report: &DistillationReport, elapsed: Duration) {
    ::metrics::counter!("aionforge_distillation_runs_total", "outcome" => "success").increment(1);
    ::metrics::histogram!("aionforge_distillation_duration_seconds", "outcome" => "success")
        .record(elapsed.as_secs_f64());
    ::metrics::counter!("aionforge_distillation_notes_written_total")
        .increment(report.notes_written as u64);
    ::metrics::counter!("aionforge_distillation_declined_total").increment(report.declined as u64);
    ::metrics::counter!("aionforge_distillation_rejected_lossy_total")
        .increment(report.rejected_lossy as u64);
    emit_guard_refusals("distill", report.guard_refused);
}

fn distillation_error(elapsed: Duration) {
    ::metrics::counter!("aionforge_distillation_runs_total", "outcome" => "error").increment(1);
    ::metrics::histogram!("aionforge_distillation_duration_seconds", "outcome" => "error")
        .record(elapsed.as_secs_f64());
}

pub(crate) fn link_evolution_result(
    result: &Result<LinkEvolveReport, LinkEvolveError>,
    elapsed: Duration,
) {
    match result {
        Ok(report) => link_evolution_report(report, elapsed),
        Err(_) => link_evolution_error(elapsed),
    }
}

fn link_evolution_report(report: &LinkEvolveReport, elapsed: Duration) {
    ::metrics::counter!("aionforge_link_evolution_runs_total", "outcome" => "success").increment(1);
    ::metrics::histogram!("aionforge_link_evolution_duration_seconds", "outcome" => "success")
        .record(elapsed.as_secs_f64());
    ::metrics::counter!("aionforge_link_evolution_links_created_total")
        .increment(report.links_created as u64);
    ::metrics::counter!("aionforge_link_evolution_links_revised_total")
        .increment(report.links_revised as u64);
    ::metrics::counter!("aionforge_link_evolution_declined_total")
        .increment(report.declined as u64);
    emit_guard_refusals("link_evolve", report.guard_refused);
}

fn link_evolution_error(elapsed: Duration) {
    ::metrics::counter!("aionforge_link_evolution_runs_total", "outcome" => "error").increment(1);
    ::metrics::histogram!("aionforge_link_evolution_duration_seconds", "outcome" => "error")
        .record(elapsed.as_secs_f64());
}

pub(crate) fn forgetting_disabled() {
    ::metrics::counter!("aionforge_forgetting_sweeps_total", "outcome" => "disabled").increment(1);
}

pub(crate) fn forgetting_sweep(report: &ForgetSweepPage, elapsed: Duration) {
    ::metrics::counter!("aionforge_forgetting_sweeps_total", "outcome" => "success").increment(1);
    ::metrics::histogram!("aionforge_forgetting_sweep_duration_seconds", "outcome" => "success")
        .record(elapsed.as_secs_f64());
    ::metrics::histogram!("aionforge_forgetting_candidates_scanned").record(report.scanned as f64);
    ::metrics::counter!("aionforge_forgetting_memories_forgotten_total")
        .increment(report.forgotten as u64);
    ::metrics::counter!("aionforge_forgetting_memories_spared_total")
        .increment(report.spared as u64);
}

pub(crate) fn forgetting_error(elapsed: Duration) {
    ::metrics::counter!("aionforge_forgetting_sweeps_total", "outcome" => "error").increment(1);
    ::metrics::histogram!("aionforge_forgetting_sweep_duration_seconds", "outcome" => "error")
        .record(elapsed.as_secs_f64());
}

pub(crate) fn drift_disabled(surface: &'static str) {
    ::metrics::counter!(
        "aionforge_drift_sweeps_total",
        "surface" => surface,
        "outcome" => "disabled",
    )
    .increment(1);
}

pub(crate) fn drift_sweep(report: &DriftSweepReport, elapsed: Duration) {
    ::metrics::counter!(
        "aionforge_drift_sweeps_total",
        "surface" => "drift",
        "outcome" => "success",
    )
    .increment(1);
    ::metrics::histogram!(
        "aionforge_drift_sweep_duration_seconds",
        "surface" => "drift",
        "outcome" => "success",
    )
    .record(elapsed.as_secs_f64());
    ::metrics::histogram!("aionforge_drift_blocks_scanned").record(report.blocks_scanned as f64);
    ::metrics::counter!("aionforge_drift_warnings_emitted_total")
        .increment(report.warnings_emitted as u64);
    ::metrics::gauge!("aionforge_drift_max_score").set(report.max_score.unwrap_or(0.0));
}

pub(crate) fn cooling_sweep(report: &CoolingSweepReport, elapsed: Duration) {
    ::metrics::counter!(
        "aionforge_drift_sweeps_total",
        "surface" => "cooling",
        "outcome" => "success",
    )
    .increment(1);
    ::metrics::histogram!(
        "aionforge_drift_sweep_duration_seconds",
        "surface" => "cooling",
        "outcome" => "success",
    )
    .record(elapsed.as_secs_f64());
    ::metrics::histogram!("aionforge_cooling_facts_scanned").record(report.facts_scanned as f64);
    ::metrics::counter!("aionforge_cooling_facts_cooled_total")
        .increment(report.facts_cooled as u64);
}

pub(crate) fn drift_error(surface: &'static str, elapsed: Duration) {
    ::metrics::counter!(
        "aionforge_drift_sweeps_total",
        "surface" => surface,
        "outcome" => "error",
    )
    .increment(1);
    ::metrics::histogram!(
        "aionforge_drift_sweep_duration_seconds",
        "surface" => surface,
        "outcome" => "error",
    )
    .record(elapsed.as_secs_f64());
}

pub(crate) fn doctor_report(store: &StoreDoctorReport, ok: bool) {
    ::metrics::gauge!("aionforge_graph_nodes").set(store.capacity.node_count as f64);
    ::metrics::gauge!("aionforge_graph_edges").set(store.capacity.edge_count as f64);
    ::metrics::gauge!("aionforge_graph_generation").set(store.capacity.generation as f64);
    ::metrics::gauge!("aionforge_doctor_ok").set(if ok { 1.0 } else { 0.0 });
}

fn emit_guard_refusals(surface: &'static str, count: usize) {
    if count > 0 {
        ::metrics::counter!("aionforge_consolidation_guard_refusals_total", "surface" => surface)
            .increment(count as u64);
    }
}

fn emit_recall_stage(class: &'static str, stage: &'static str, millis: u128) {
    ::metrics::histogram!(
        "aionforge_recall_stage_duration_seconds",
        "class" => class,
        "stage" => stage,
    )
    .record(millis as f64 / 1000.0);
}

fn capture_verdict_label(verdict: &CaptureVerdict) -> &'static str {
    match verdict {
        CaptureVerdict::New => "new",
        CaptureVerdict::ExactDuplicate => "exact_duplicate",
        CaptureVerdict::NearDuplicate { .. } => "near_duplicate",
    }
}

fn embedding_label(outcome: &EmbeddingOutcome) -> &'static str {
    match outcome {
        EmbeddingOutcome::Embedded => "embedded",
        EmbeddingOutcome::NotRequested => "not_requested",
    }
}

fn capture_error_label(error: &CaptureError) -> &'static str {
    match error {
        CaptureError::Filter(_) => "filter",
        CaptureError::Store(_) => "store",
        CaptureError::Embedder(_) => "embedder",
        CaptureError::Unauthorized(_) => "unauthorized",
        CaptureError::InvalidSignature => "invalid_signature",
        CaptureError::ClockSkew { .. } => "clock_skew",
        CaptureError::ProvenanceUnavailable(_) => "provenance_unavailable",
        CaptureError::SystemRoleNotWritable => "system_role_not_writable",
        _ => "other",
    }
}

pub(crate) fn retrieval_error_label(error: &aionforge_retrieval::RetrievalError) -> &'static str {
    match error {
        aionforge_retrieval::RetrievalError::Store(_) => "store",
        aionforge_retrieval::RetrievalError::DeadlineExceeded => "deadline_exceeded",
        _ => "other",
    }
}

fn query_class_label(class: QueryClass) -> &'static str {
    match class {
        QueryClass::SingleHopFactual => "single_hop_factual",
        QueryClass::MultiHop => "multi_hop",
        QueryClass::Temporal => "temporal",
        QueryClass::Entity => "entity",
        QueryClass::Quote => "quote",
    }
}
