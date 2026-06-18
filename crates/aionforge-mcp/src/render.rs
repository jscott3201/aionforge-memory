//! Shared compact text renderers for memory-shaped MCP read output.

use aionforge_domain::ids::Id;
use aionforge_domain::nodes::core::BlockKind;
use aionforge_domain::nodes::episodic::Episode;
use aionforge_domain::nodes::forensic::ProvenanceRecord;
use aionforge_domain::nodes::semantic::FactStatus;
use aionforge_domain::nodes::work::WorkStatus;
use aionforge_engine::ResolvedMemory;

pub(crate) fn render_episode_line(
    episode: &Episode,
    superseded_by: Option<&Id>,
    provenance: Option<&ProvenanceRecord>,
    max_chars: usize,
) -> String {
    let supersedes = episode.origin.as_ref().and_then(|origin| origin.supersedes);
    format!(
        "<memory id=\"{}\" kind=\"episode\" ns=\"{}\" role=\"{}\" captured_at=\"{}\" ingested_at=\"{}\" session=\"{}\" supersedes=\"{}\" superseded_by=\"{}\"{}>{}</memory>",
        attr_escape(&episode.identity.id.to_string()),
        attr_escape(&episode.identity.namespace.to_string()),
        role_name(episode),
        attr_escape(&episode.captured_at.to_string()),
        attr_escape(&episode.identity.ingested_at.to_string()),
        attr_escape(&render_optional_id(episode.session_id.as_ref())),
        attr_escape(&render_optional_id(supersedes.as_ref())),
        attr_escape(&render_optional_id(superseded_by)),
        provenance_attrs(provenance),
        tag_escape(&truncate_chars(&episode.content, max_chars))
    )
}

fn provenance_attrs(provenance: Option<&ProvenanceRecord>) -> String {
    let Some(record) = provenance else {
        return String::new();
    };
    format!(
        " writer=\"{}\" model_family=\"{}\" model_version=\"{}\" trust_at_write=\"{:.2}\" written_at=\"{}\"",
        attr_escape(&record.writer_agent_id.to_string()),
        attr_escape(&record.model_family),
        attr_escape(record.model_version.as_deref().unwrap_or("none")),
        record.trust_at_write,
        attr_escape(&record.identity.ingested_at.to_string()),
    )
}

pub(crate) fn render_memory_line(
    memory: &ResolvedMemory,
    superseded_by: Option<&Id>,
    provenance: Option<&ProvenanceRecord>,
    max_chars: usize,
) -> String {
    match memory {
        ResolvedMemory::Episode(episode) => {
            render_episode_line(episode, superseded_by, provenance, max_chars)
        }
        ResolvedMemory::Fact(fact) => format!(
            "<memory id=\"{}\" kind=\"fact\" ns=\"{}\" ingested_at=\"{}\" predicate=\"{}\" status=\"{}\">{}</memory>",
            attr_escape(&fact.identity.id.to_string()),
            attr_escape(&fact.identity.namespace.to_string()),
            attr_escape(&fact.identity.ingested_at.to_string()),
            attr_escape(&fact.predicate),
            fact_status_tag(fact.status),
            tag_escape(&truncate_chars(&fact.statement, max_chars)),
        ),
        ResolvedMemory::Entity(entity) => {
            let mut body = entity.canonical_name.clone();
            if let Some(description) = &entity.description {
                body.push_str(" — ");
                body.push_str(description);
            }
            format!(
                "<memory id=\"{}\" kind=\"entity\" ns=\"{}\" ingested_at=\"{}\" entity_type=\"{}\">{}</memory>",
                attr_escape(&entity.identity.id.to_string()),
                attr_escape(&entity.identity.namespace.to_string()),
                attr_escape(&entity.identity.ingested_at.to_string()),
                attr_escape(&entity.entity_type),
                tag_escape(&truncate_chars(&body, max_chars)),
            )
        }
        ResolvedMemory::Note(note) => format!(
            "<memory id=\"{}\" kind=\"note\" ns=\"{}\" ingested_at=\"{}\">{}</memory>",
            attr_escape(&note.identity.id.to_string()),
            attr_escape(&note.identity.namespace.to_string()),
            attr_escape(&note.identity.ingested_at.to_string()),
            tag_escape(&truncate_chars(&note.content, max_chars)),
        ),
        ResolvedMemory::Skill(skill) => format!(
            "<memory id=\"{}\" kind=\"skill\" ns=\"{}\" ingested_at=\"{}\" name=\"{}\" version=\"{}\" deprecated=\"{}\">{}</memory>",
            attr_escape(&skill.identity.id.to_string()),
            attr_escape(&skill.identity.namespace.to_string()),
            attr_escape(&skill.identity.ingested_at.to_string()),
            attr_escape(&skill.name),
            skill.version,
            skill.deprecated_at.is_some(),
            tag_escape(&truncate_chars(&skill.description, max_chars)),
        ),
        ResolvedMemory::BadPattern(pattern) => format!(
            "<memory id=\"{}\" kind=\"bad_pattern\" ns=\"{}\" ingested_at=\"{}\" observed_at=\"{}\">{}</memory>",
            attr_escape(&pattern.identity.id.to_string()),
            attr_escape(&pattern.identity.namespace.to_string()),
            attr_escape(&pattern.identity.ingested_at.to_string()),
            attr_escape(&pattern.observed_at.to_string()),
            tag_escape(&truncate_chars(&pattern.description, max_chars)),
        ),
        ResolvedMemory::Core(core) => format!(
            "<memory id=\"{}\" kind=\"core\" ns=\"{}\" ingested_at=\"{}\" block_kind=\"{}\">{}</memory>",
            attr_escape(&core.identity.id.to_string()),
            attr_escape(&core.identity.namespace.to_string()),
            attr_escape(&core.identity.ingested_at.to_string()),
            block_kind_tag(core.block_kind),
            tag_escape(&truncate_chars(&core.content, max_chars)),
        ),
        ResolvedMemory::WorkItem(item) => {
            let mut body = item.title.clone();
            if let Some(detail) = &item.body {
                body.push_str(" — ");
                body.push_str(detail);
            }
            format!(
                "<memory id=\"{}\" kind=\"work_item\" ns=\"{}\" ingested_at=\"{}\" level=\"{}\" work_status=\"{}\" parent=\"{}\" ordinal=\"{}\">{}</memory>",
                attr_escape(&item.identity.id.to_string()),
                attr_escape(&item.identity.namespace.to_string()),
                attr_escape(&item.identity.ingested_at.to_string()),
                attr_escape(&item.level),
                work_status_tag(item.work_status),
                attr_escape(&render_optional_id(item.parent_id.as_ref())),
                item.ordinal,
                tag_escape(&truncate_chars(&body, max_chars)),
            )
        }
        ResolvedMemory::Tag(tag) => format!(
            "<memory id=\"{}\" kind=\"tag\" ns=\"{}\" ingested_at=\"{}\" slug=\"{}\">{}</memory>",
            attr_escape(&tag.identity.id.to_string()),
            attr_escape(&tag.identity.namespace.to_string()),
            attr_escape(&tag.identity.ingested_at.to_string()),
            attr_escape(&tag.slug),
            tag_escape(&truncate_chars(
                tag.display.as_deref().unwrap_or(&tag.slug),
                max_chars
            )),
        ),
    }
}

pub(crate) fn work_status_tag(status: WorkStatus) -> &'static str {
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

fn render_optional_id(id: Option<&Id>) -> String {
    id.map(ToString::to_string)
        .unwrap_or_else(|| "none".to_string())
}

fn role_name(episode: &Episode) -> &'static str {
    match episode.role {
        aionforge_domain::nodes::episodic::Role::User => "user",
        aionforge_domain::nodes::episodic::Role::Assistant => "assistant",
        aionforge_domain::nodes::episodic::Role::Tool => "tool",
        aionforge_domain::nodes::episodic::Role::System => "system",
        aionforge_domain::nodes::episodic::Role::Event => "event",
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut out: String = value.chars().take(max_chars).collect();
    out.push_str("...");
    out
}

fn tag_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn attr_escape(value: &str) -> String {
    tag_escape(value).replace('"', "&quot;")
}
