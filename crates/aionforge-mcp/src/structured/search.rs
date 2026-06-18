//! Structured DTO builder for the `search` tool.

use std::collections::BTreeMap;

use aionforge_domain::nodes::core::BlockKind;
use aionforge_domain::nodes::episodic::Role;
use aionforge_domain::nodes::semantic::FactStatus;
use aionforge_engine::{QueryClass, RecallBundle, Signal, SignalWeights, StructuredEntry};
use serde::Serialize;

use super::StructuredToolOutput;

const COMPACT_SNIPPET_CHARS: usize = 360;

#[derive(Serialize)]
struct SearchResultsStructured {
    schema: &'static str,
    summary: SearchSummaryStructured,
    explain: SearchExplainStructured,
    memories: Vec<SearchMemoryStructured>,
}

#[derive(Serialize)]
struct SearchSummaryStructured {
    returned: usize,
    candidates_considered: usize,
    filtered_or_hidden: usize,
    query_class: &'static str,
    embedder_available: bool,
}

#[derive(Serialize)]
struct SearchExplainStructured {
    route: &'static str,
    signals_run: Vec<&'static str>,
    weights: BTreeMap<&'static str, f64>,
}

#[derive(Serialize)]
struct SearchSignalStructured {
    signal: &'static str,
    rank: usize,
    weight: f64,
}

#[derive(Serialize)]
struct SearchMemoryStructured {
    id: String,
    serialization_id: String,
    kind: &'static str,
    namespace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    predicate: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    block_kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    score_band: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dense_similarity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence_band: Option<&'static str>,
    trust: f64,
    signals: Vec<SearchSignalStructured>,
    #[serde(skip_serializing_if = "Option::is_none")]
    supersedes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_by: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    always: bool,
    snippet: String,
}

/// Attach the structured search DTO to the already-rendered compact text.
pub(crate) fn output(text: String, bundle: &RecallBundle) -> StructuredToolOutput {
    StructuredToolOutput::new(text, structured_search_results(bundle))
}

fn structured_search_results(bundle: &RecallBundle) -> SearchResultsStructured {
    let explanation = &bundle.explanation;
    let max_ranked_score = bundle
        .structured
        .iter()
        .filter(|entry| !matches!(entry, StructuredEntry::CoreBlock(_)))
        .map(StructuredEntry::score)
        .fold(0.0_f64, f64::max);
    SearchResultsStructured {
        schema: "aionforge.search_results.v1",
        summary: SearchSummaryStructured {
            returned: explanation.returned,
            candidates_considered: explanation.candidates_considered,
            filtered_or_hidden: explanation
                .candidates_considered
                .saturating_sub(explanation.returned),
            query_class: query_class_tag(explanation.class),
            embedder_available: explanation.embedder_available,
        },
        explain: SearchExplainStructured {
            route: query_class_tag(explanation.class),
            signals_run: explanation
                .signals_run
                .iter()
                .map(|signal| signal_tag(*signal))
                .collect(),
            weights: signal_weight_map(&explanation.weights, &explanation.signals_run),
        },
        memories: bundle
            .structured
            .iter()
            .map(|entry| structured_search_memory(entry, max_ranked_score))
            .collect(),
    }
}

fn structured_search_memory(
    entry: &StructuredEntry,
    max_ranked_score: f64,
) -> SearchMemoryStructured {
    let mut memory = SearchMemoryStructured {
        id: entry.id().to_string(),
        serialization_id: entry.serialization_id().to_string(),
        kind: "episode",
        namespace: entry.namespace().to_string(),
        role: None,
        predicate: None,
        status: None,
        block_kind: None,
        score: Some(entry.score()),
        score_band: Some(search_score_band(entry.score(), max_ranked_score)),
        dense_similarity: entry.dense_similarity(),
        confidence_band: entry.dense_similarity().map(search_confidence_band),
        trust: entry.trust(),
        signals: entry
            .contributions()
            .iter()
            .map(|contribution| SearchSignalStructured {
                signal: signal_tag(contribution.signal),
                rank: contribution.rank,
                weight: contribution.weight,
            })
            .collect(),
        supersedes: None,
        superseded_by: None,
        always: false,
        snippet: compact_snippet(entry.content(), COMPACT_SNIPPET_CHARS),
    };
    match entry {
        StructuredEntry::Episode(episode) => {
            memory.role = Some(role_tag(episode.role));
            memory.supersedes = episode.supersedes.map(|id| id.to_string());
            memory.superseded_by = episode.superseded_by.map(|id| id.to_string());
        }
        StructuredEntry::Fact(fact) => {
            memory.kind = "fact";
            memory.predicate = Some(fact.predicate.clone());
            memory.status = Some(fact_status_tag(fact.status));
        }
        StructuredEntry::CoreBlock(core) => {
            memory.kind = "core";
            memory.block_kind = Some(block_kind_tag(core.block_kind));
            memory.score = None;
            memory.score_band = None;
            memory.always = true;
        }
    }
    memory
}

fn signal_weight_map(
    weights: &SignalWeights,
    signals_run: &[Signal],
) -> BTreeMap<&'static str, f64> {
    signals_run
        .iter()
        .map(|signal| (signal_tag(*signal), weights.weight(*signal)))
        .filter(|(_, weight)| *weight > 0.0)
        .collect()
}

fn query_class_tag(class: QueryClass) -> &'static str {
    match class {
        QueryClass::SingleHopFactual => "single_hop_factual",
        QueryClass::MultiHop => "multi_hop",
        QueryClass::Temporal => "temporal",
        QueryClass::Entity => "entity",
        QueryClass::Quote => "quote",
    }
}

fn signal_tag(signal: Signal) -> &'static str {
    match signal {
        Signal::Lexical => "lexical",
        Signal::LexicalAnchor => "lexical_anchor",
        Signal::Dense => "dense",
        Signal::Support => "support",
        Signal::Graph => "graph",
        Signal::Authority => "authority",
        Signal::Recency => "recency",
        Signal::Importance => "importance",
        Signal::Trust => "trust",
    }
}

fn role_tag(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::System => "system",
        Role::Event => "event",
    }
}

fn fact_status_tag(status: FactStatus) -> &'static str {
    match status {
        FactStatus::Active => "active",
        FactStatus::Quarantined => "quarantined",
        FactStatus::Superseded => "superseded",
    }
}

fn block_kind_tag(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Persona => "persona",
        BlockKind::Commitment => "commitment",
        BlockKind::Redline => "redline",
    }
}

fn search_score_band(score: f64, max_score: f64) -> &'static str {
    if max_score <= 0.0 || score <= 0.0 {
        return "low";
    }
    let ratio = score / max_score;
    if ratio >= 0.85 {
        "high"
    } else if ratio >= 0.50 {
        "medium"
    } else {
        "low"
    }
}

fn search_confidence_band(similarity: f64) -> &'static str {
    if similarity >= 0.62 {
        "high"
    } else if similarity >= 0.45 {
        "medium"
    } else {
        "low"
    }
}

fn compact_snippet(content: &str, max: usize) -> String {
    let collapsed = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        collapsed
    } else {
        let head: String = collapsed.chars().take(max).collect();
        format!("{head}...")
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}
