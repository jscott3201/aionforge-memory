//! Structured DTO builder for `memory_census`.

use std::collections::HashMap;

use aionforge_domain::ids::Id;
use aionforge_engine::{MemoryCensusReport, MemoryCounts, ResolvedMemory, WorkCounts};
use serde::Serialize;

use super::StructuredToolOutput;
use super::inspect::{MemoryRecord, memory_record};

#[derive(Serialize)]
struct MemoryCensusStructured {
    schema: &'static str,
    mode: &'static str,
    namespaces: Vec<NamespaceCensusStructured>,
    totals: MemoryCensusTotalsStructured,
    #[serde(skip_serializing_if = "Option::is_none")]
    list: Option<MemoryCensusListStructured>,
}

#[derive(Serialize)]
struct NamespaceCensusStructured {
    namespace: String,
    kinds: MemoryKindCountsStructured,
    work_statuses: WorkStatusCountsStructured,
    total: u64,
}

#[derive(Serialize)]
struct MemoryCensusTotalsStructured {
    memories: u64,
    work_items: u64,
    kinds: MemoryKindCountsStructured,
    work_statuses: WorkStatusCountsStructured,
}

#[derive(Serialize)]
struct MemoryKindCountsStructured {
    episodes: u64,
    facts: u64,
    entities: u64,
    notes: u64,
    skills: u64,
    bad_patterns: u64,
}

#[derive(Serialize)]
struct WorkStatusCountsStructured {
    todo: u64,
    in_progress: u64,
    blocked: u64,
    done: u64,
    dropped: u64,
}

#[derive(Serialize)]
struct MemoryCensusListStructured {
    count: usize,
    total_visible: usize,
    limit: usize,
    next: Option<MemoryCensusCursorStructured>,
    memories: Vec<MemoryRecord>,
}

#[derive(Serialize)]
struct MemoryCensusCursorStructured {
    ingested_at: String,
    id: String,
}

/// Inputs for attaching a structured `memory_census` DTO to already-rendered text.
pub(crate) struct MemoryCensusOutput<'a> {
    /// The compact text response already rendered for the tool.
    pub(crate) text: String,
    /// The selected census mode.
    pub(crate) mode: &'static str,
    /// Count report for the visible namespace set.
    pub(crate) report: &'a MemoryCensusReport,
    /// Optional list-mode payload.
    pub(crate) list: Option<MemoryCensusListOutput<'a>>,
}

/// Inputs for the optional list-mode section of the structured `memory_census` DTO.
pub(crate) struct MemoryCensusListOutput<'a> {
    /// Page of visible memories after cursor and limit.
    pub(crate) memories: &'a [ResolvedMemory],
    /// Live episode replacement ids keyed by superseded episode id.
    pub(crate) superseded_by: &'a HashMap<Id, Id>,
    /// Requested result limit.
    pub(crate) limit: usize,
    /// Total visible records after cursor filtering and before page truncation.
    pub(crate) total_visible: usize,
    /// Next-page cursor as `(ingested_at, id)`, if another page exists.
    pub(crate) next: Option<(String, String)>,
    /// Maximum characters retained in rendered bodies.
    pub(crate) max_chars: usize,
}

/// Attach a structured `memory_census` DTO to the already-rendered text.
pub(crate) fn memory_census(input: MemoryCensusOutput<'_>) -> StructuredToolOutput {
    StructuredToolOutput::new(
        input.text,
        MemoryCensusStructured {
            schema: "aionforge.memory_census.v1",
            mode: input.mode,
            namespaces: input
                .report
                .namespaces
                .iter()
                .map(|namespace| NamespaceCensusStructured {
                    namespace: namespace.namespace.to_string(),
                    kinds: memory_kinds(namespace.memories),
                    work_statuses: work_statuses(namespace.work_items),
                    total: namespace.memories.total() + namespace.work_items.total(),
                })
                .collect(),
            totals: totals(input.report),
            list: input.list.map(|list| MemoryCensusListStructured {
                count: list.memories.len(),
                total_visible: list.total_visible,
                limit: list.limit,
                next: list
                    .next
                    .map(|(ingested_at, id)| MemoryCensusCursorStructured { ingested_at, id }),
                memories: list
                    .memories
                    .iter()
                    .map(|record| {
                        let superseded = match record {
                            ResolvedMemory::Episode(episode) => {
                                list.superseded_by.get(&episode.identity.id)
                            }
                            _ => None,
                        };
                        memory_record(record, superseded, None, list.max_chars)
                    })
                    .collect(),
            }),
        },
    )
}

fn totals(report: &MemoryCensusReport) -> MemoryCensusTotalsStructured {
    let mut memories = MemoryCounts::default();
    let mut work_items = WorkCounts::default();
    for namespace in &report.namespaces {
        memories.episodes += namespace.memories.episodes;
        memories.facts += namespace.memories.facts;
        memories.entities += namespace.memories.entities;
        memories.notes += namespace.memories.notes;
        memories.skills += namespace.memories.skills;
        memories.bad_patterns += namespace.memories.bad_patterns;
        work_items.todo += namespace.work_items.todo;
        work_items.in_progress += namespace.work_items.in_progress;
        work_items.blocked += namespace.work_items.blocked;
        work_items.done += namespace.work_items.done;
        work_items.dropped += namespace.work_items.dropped;
    }
    MemoryCensusTotalsStructured {
        memories: memories.total(),
        work_items: work_items.total(),
        kinds: memory_kinds(memories),
        work_statuses: work_statuses(work_items),
    }
}

fn memory_kinds(counts: MemoryCounts) -> MemoryKindCountsStructured {
    MemoryKindCountsStructured {
        episodes: counts.episodes,
        facts: counts.facts,
        entities: counts.entities,
        notes: counts.notes,
        skills: counts.skills,
        bad_patterns: counts.bad_patterns,
    }
}

fn work_statuses(counts: WorkCounts) -> WorkStatusCountsStructured {
    WorkStatusCountsStructured {
        todo: counts.todo,
        in_progress: counts.in_progress,
        blocked: counts.blocked,
        done: counts.done,
        dropped: counts.dropped,
    }
}
