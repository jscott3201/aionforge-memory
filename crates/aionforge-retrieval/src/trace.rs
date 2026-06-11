//! Low-cardinality tracing helpers for recall.

use crate::{QueryClass, RecallBundle, RecallQuery, RetrievalError, Signal, TemporalMode};

pub(crate) fn recall_span(query: &RecallQuery) -> tracing::Span {
    tracing::info_span!(
        "aionforge.recall",
        class = tracing::field::Empty,
        temporal = temporal_label(&query.options.temporal),
        sensitive = query.options.sensitive,
        include_expired = query.options.include_expired,
        include_system = query.options.include_system,
        mode_override = query.options.mode_override.is_some(),
        deadline = query.options.deadline.is_some(),
        fanout = query.options.fanout as u64,
        limit = query.limit as u64,
        outcome = tracing::field::Empty,
        embedder = tracing::field::Empty,
        error = tracing::field::Empty,
        returned = tracing::field::Empty,
        candidates_considered = tracing::field::Empty,
        signals_run = tracing::field::Empty,
    )
}

pub(crate) fn record_recall_result(
    span: &tracing::Span,
    result: &Result<RecallBundle, RetrievalError>,
) {
    match result {
        Ok(bundle) => {
            span.record("outcome", "success");
            span.record("class", query_class_label(bundle.explanation.class));
            span.record(
                "embedder",
                if bundle.explanation.embedder_available {
                    "available"
                } else {
                    "unavailable"
                },
            );
            span.record("error", "none");
            span.record("returned", bundle.explanation.returned as u64);
            span.record(
                "candidates_considered",
                bundle.explanation.candidates_considered as u64,
            );
            span.record("signals_run", bundle.explanation.signals_run.len() as u64);
        }
        Err(error) => {
            span.record("outcome", "error");
            span.record("class", "unknown");
            span.record("embedder", "unknown");
            span.record("error", retrieval_error_label(error));
        }
    }
}

pub(crate) fn stage_span(stage: &'static str) -> tracing::Span {
    tracing::info_span!("aionforge.recall.stage", stage = stage)
}

pub(crate) fn signal_span(signal: Signal, fanout: usize) -> tracing::Span {
    tracing::info_span!(
        "aionforge.recall.signal",
        signal = signal_label(signal),
        fanout = fanout as u64,
    )
}

pub(crate) fn query_embed_span(fanout: usize) -> tracing::Span {
    tracing::info_span!(
        "aionforge.recall.signal",
        signal = "query_embed",
        fanout = fanout as u64,
    )
}

fn temporal_label(mode: &TemporalMode) -> &'static str {
    match mode {
        TemporalMode::Current => "current",
        TemporalMode::AsOf(_) => "as_of",
        TemporalMode::AsKnownAt(_) => "as_known_at",
        TemporalMode::History => "history",
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

fn signal_label(signal: Signal) -> &'static str {
    match signal {
        Signal::Lexical => "lexical",
        Signal::LexicalAnchor => "lexical_anchor",
        Signal::Dense => "dense",
        Signal::Support => "support",
        Signal::Graph => "graph",
        Signal::Recency => "recency",
        Signal::Importance => "importance",
        Signal::Trust => "trust",
    }
}

fn retrieval_error_label(error: &RetrievalError) -> &'static str {
    match error {
        RetrievalError::Store(_) => "store",
        RetrievalError::DeadlineExceeded => "deadline_exceeded",
    }
}
