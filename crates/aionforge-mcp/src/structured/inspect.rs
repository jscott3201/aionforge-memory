//! Structured DTO builders for `read_memory` and `session_manifest`.

use std::collections::HashMap;

use aionforge_domain::ids::Id;
use aionforge_domain::nodes::core::BlockKind;
use aionforge_domain::nodes::episodic::{Episode, Role};
use aionforge_domain::nodes::forensic::ProvenanceRecord;
use aionforge_domain::nodes::semantic::FactStatus;
use aionforge_domain::nodes::work::WorkStatus;
use aionforge_engine::ResolvedMemory;
use serde::Serialize;

use super::StructuredToolOutput;

#[derive(Serialize)]
struct ReadMemoryStructured {
    schema: &'static str,
    requested: usize,
    found: usize,
    memories: Vec<MemoryRecord>,
    missing_or_unauthorized: usize,
}

#[derive(Serialize)]
struct SessionManifestStructured {
    schema: &'static str,
    session_id: String,
    count: usize,
    total_visible: usize,
    limit: usize,
    superseded_hidden: usize,
    next: Option<SessionManifestCursorStructured>,
    episodes: Vec<MemoryRecord>,
}

#[derive(Serialize)]
struct SessionManifestCursorStructured {
    ingested_at: String,
    id: String,
}

#[derive(Serialize)]
struct MemoryProvenance {
    writer: String,
    model_family: String,
    model_version: Option<String>,
    trust_at_write: f64,
    written_at: String,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum MemoryRecord {
    Episode {
        id: String,
        namespace: String,
        ingested_at: String,
        captured_at: String,
        role: &'static str,
        session_id: Option<String>,
        supersedes: Option<String>,
        superseded_by: Option<String>,
        provenance: Option<MemoryProvenance>,
        body: String,
        body_truncated: bool,
    },
    Fact {
        id: String,
        namespace: String,
        ingested_at: String,
        predicate: String,
        status: &'static str,
        statement: String,
        statement_truncated: bool,
    },
    Entity {
        id: String,
        namespace: String,
        ingested_at: String,
        entity_type: String,
        canonical_name: String,
        description: Option<String>,
        body: String,
        body_truncated: bool,
    },
    Note {
        id: String,
        namespace: String,
        ingested_at: String,
        content: String,
        content_truncated: bool,
    },
    Skill {
        id: String,
        namespace: String,
        ingested_at: String,
        name: String,
        version: i64,
        deprecated: bool,
        description: String,
        description_truncated: bool,
    },
    BadPattern {
        id: String,
        namespace: String,
        ingested_at: String,
        observed_at: String,
        description: String,
        description_truncated: bool,
    },
    Core {
        id: String,
        namespace: String,
        ingested_at: String,
        block_kind: &'static str,
        content: String,
        content_truncated: bool,
    },
    WorkItem {
        id: String,
        namespace: String,
        ingested_at: String,
        level: String,
        work_status: &'static str,
        parent: Option<String>,
        ordinal: u64,
        title: String,
        body: Option<String>,
        display: String,
        display_truncated: bool,
    },
    Tag {
        id: String,
        namespace: String,
        ingested_at: String,
        slug: String,
        display: String,
        display_truncated: bool,
    },
}

/// Attach a structured read-memory DTO to the already-rendered text.
pub(crate) fn read_memory(
    text: String,
    requested: usize,
    found_memories: &[ResolvedMemory],
    superseded_by: &HashMap<Id, Id>,
    provenance: &HashMap<Id, ProvenanceRecord>,
    max_chars: usize,
) -> StructuredToolOutput {
    StructuredToolOutput::new(
        text,
        ReadMemoryStructured {
            schema: "aionforge.read_memory.v1",
            requested,
            found: found_memories.len(),
            missing_or_unauthorized: requested.saturating_sub(found_memories.len()),
            memories: found_memories
                .iter()
                .map(|resolved| {
                    let (superseded, prov) = match resolved {
                        ResolvedMemory::Episode(episode) => (
                            superseded_by.get(&episode.identity.id).copied(),
                            provenance.get(&episode.identity.id),
                        ),
                        _ => (None, None),
                    };
                    memory_record(resolved, superseded.as_ref(), prov, max_chars)
                })
                .collect(),
        },
    )
}

/// Attach a structured session-manifest DTO to the already-rendered text.
pub(crate) fn session_manifest(
    text: String,
    session_id: &Id,
    episodes: &[(Episode, Option<Id>)],
    limit: usize,
    total_visible: usize,
    superseded_hidden: usize,
    next: Option<(String, String)>,
    max_chars: usize,
) -> StructuredToolOutput {
    StructuredToolOutput::new(
        text,
        SessionManifestStructured {
            schema: "aionforge.session_manifest.v1",
            session_id: session_id.to_string(),
            count: episodes.len(),
            total_visible,
            limit,
            superseded_hidden,
            next: next.map(|(ingested_at, id)| SessionManifestCursorStructured { ingested_at, id }),
            episodes: episodes
                .iter()
                .map(|(episode, superseded_by)| {
                    memory_record(
                        &ResolvedMemory::Episode(episode.clone()),
                        superseded_by.as_ref(),
                        None,
                        max_chars,
                    )
                })
                .collect(),
        },
    )
}

fn memory_record(
    memory: &ResolvedMemory,
    superseded_by: Option<&Id>,
    provenance: Option<&ProvenanceRecord>,
    max_chars: usize,
) -> MemoryRecord {
    match memory {
        ResolvedMemory::Episode(episode) => {
            let (body, body_truncated) = truncate_with_flag(&episode.content, max_chars);
            MemoryRecord::Episode {
                id: episode.identity.id.to_string(),
                namespace: episode.identity.namespace.to_string(),
                ingested_at: episode.identity.ingested_at.to_string(),
                captured_at: episode.captured_at.to_string(),
                role: role_tag(episode.role),
                session_id: episode.session_id.map(|id| id.to_string()),
                supersedes: episode
                    .origin
                    .as_ref()
                    .and_then(|origin| origin.supersedes)
                    .map(|id| id.to_string()),
                superseded_by: superseded_by.map(ToString::to_string),
                provenance: provenance.map(structured_provenance),
                body,
                body_truncated,
            }
        }
        ResolvedMemory::Fact(fact) => {
            let (statement, statement_truncated) = truncate_with_flag(&fact.statement, max_chars);
            MemoryRecord::Fact {
                id: fact.identity.id.to_string(),
                namespace: fact.identity.namespace.to_string(),
                ingested_at: fact.identity.ingested_at.to_string(),
                predicate: fact.predicate.clone(),
                status: fact_status_tag(fact.status),
                statement,
                statement_truncated,
            }
        }
        ResolvedMemory::Entity(entity) => {
            let mut body_raw = entity.canonical_name.clone();
            if let Some(description) = &entity.description {
                body_raw.push_str(" — ");
                body_raw.push_str(description);
            }
            let (body, body_truncated) = truncate_with_flag(&body_raw, max_chars);
            MemoryRecord::Entity {
                id: entity.identity.id.to_string(),
                namespace: entity.identity.namespace.to_string(),
                ingested_at: entity.identity.ingested_at.to_string(),
                entity_type: entity.entity_type.clone(),
                canonical_name: entity.canonical_name.clone(),
                description: entity.description.clone(),
                body,
                body_truncated,
            }
        }
        ResolvedMemory::Note(note) => {
            let (content, content_truncated) = truncate_with_flag(&note.content, max_chars);
            MemoryRecord::Note {
                id: note.identity.id.to_string(),
                namespace: note.identity.namespace.to_string(),
                ingested_at: note.identity.ingested_at.to_string(),
                content,
                content_truncated,
            }
        }
        ResolvedMemory::Skill(skill) => {
            let (description, description_truncated) =
                truncate_with_flag(&skill.description, max_chars);
            MemoryRecord::Skill {
                id: skill.identity.id.to_string(),
                namespace: skill.identity.namespace.to_string(),
                ingested_at: skill.identity.ingested_at.to_string(),
                name: skill.name.clone(),
                version: skill.version,
                deprecated: skill.deprecated_at.is_some(),
                description,
                description_truncated,
            }
        }
        ResolvedMemory::BadPattern(pattern) => {
            let (description, description_truncated) =
                truncate_with_flag(&pattern.description, max_chars);
            MemoryRecord::BadPattern {
                id: pattern.identity.id.to_string(),
                namespace: pattern.identity.namespace.to_string(),
                ingested_at: pattern.identity.ingested_at.to_string(),
                observed_at: pattern.observed_at.to_string(),
                description,
                description_truncated,
            }
        }
        ResolvedMemory::Core(core) => {
            let (content, content_truncated) = truncate_with_flag(&core.content, max_chars);
            MemoryRecord::Core {
                id: core.identity.id.to_string(),
                namespace: core.identity.namespace.to_string(),
                ingested_at: core.identity.ingested_at.to_string(),
                block_kind: block_kind_tag(core.block_kind),
                content,
                content_truncated,
            }
        }
        ResolvedMemory::WorkItem(item) => {
            let mut display_raw = item.title.clone();
            if let Some(detail) = &item.body {
                display_raw.push_str(" — ");
                display_raw.push_str(detail);
            }
            let (display, display_truncated) = truncate_with_flag(&display_raw, max_chars);
            MemoryRecord::WorkItem {
                id: item.identity.id.to_string(),
                namespace: item.identity.namespace.to_string(),
                ingested_at: item.identity.ingested_at.to_string(),
                level: item.level.clone(),
                work_status: work_status_tag(item.work_status),
                parent: item.parent_id.map(|id| id.to_string()),
                ordinal: item.ordinal,
                title: item.title.clone(),
                body: item.body.clone(),
                display,
                display_truncated,
            }
        }
        ResolvedMemory::Tag(tag) => {
            let raw = tag.display.as_deref().unwrap_or(&tag.slug);
            let (display, display_truncated) = truncate_with_flag(raw, max_chars);
            MemoryRecord::Tag {
                id: tag.identity.id.to_string(),
                namespace: tag.identity.namespace.to_string(),
                ingested_at: tag.identity.ingested_at.to_string(),
                slug: tag.slug.clone(),
                display,
                display_truncated,
            }
        }
    }
}

fn structured_provenance(record: &ProvenanceRecord) -> MemoryProvenance {
    MemoryProvenance {
        writer: record.writer_agent_id.to_string(),
        model_family: record.model_family.clone(),
        model_version: record.model_version.clone(),
        trust_at_write: record.trust_at_write,
        written_at: record.identity.ingested_at.to_string(),
    }
}

fn work_status_tag(status: WorkStatus) -> &'static str {
    match status {
        WorkStatus::Todo => "todo",
        WorkStatus::InProgress => "in_progress",
        WorkStatus::Blocked => "blocked",
        WorkStatus::Done => "done",
        WorkStatus::Dropped => "dropped",
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

fn role_tag(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::System => "system",
        Role::Event => "event",
    }
}

fn truncate_with_flag(value: &str, max_chars: usize) -> (String, bool) {
    let truncated = value.chars().count() > max_chars;
    (truncate_chars(value, max_chars), truncated)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut out: String = value.chars().take(max_chars).collect();
    out.push_str("...");
    out
}
