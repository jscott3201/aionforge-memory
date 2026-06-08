//! Deterministic audit attribution for the consolidation passes (write-and-consolidation
//! §3, 04 §3).
//!
//! Two properties this module exists to guarantee:
//!
//! - **Stable actor identity.** A pass's `actor_id` is a content hash over the pass's
//!   configuration (extractor, embedder, and summarizer identities), not a per-process
//!   random value. Two runs of the same pass configuration attribute their decisions to the
//!   same actor, so a forensic query can ask "which pass version made this decision" and get
//!   a stable answer across restarts.
//! - **Replay-idempotent audit ids.** An audit event's id is a content hash over the
//!   identifying content of the decision it records (the episode it ran for, the surface or
//!   subject or fact it concerns). Re-running the same episode — the crash-recovery path, or
//!   an explicit cursor reset — yields the same ids, so the dedup-aware audit write makes the
//!   replay a no-op, exactly like the content-derived fact and note ids. Distinct decisions
//!   still get distinct ids.

use aionforge_domain::blocks::Identity;
use aionforge_domain::contracts::{
    EntitySurface, ExtractorIdentity, InducerIdentity, SummarizationCluster, SummarizerIdentity,
};
use aionforge_domain::embedding::EmbedderModel;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::Episode;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::procedural::Skill;
use aionforge_domain::time::Timestamp;
use serde_json::json;

use crate::resolve::Resolution;
use crate::summarize::RetentionOutcome;

/// The deterministic actor id stamped on a [`FactExtractionPass`](crate::FactExtractionPass)'s
/// audit events: a content hash over the pass's identity — the extractor rule version and
/// model identity, the embedder family/version, and the summarizer rule version. The same
/// pass configuration always attributes its decisions to the same actor, even across process
/// restarts (replacing the old per-process random id, which made forensic attribution and
/// exact-replay tests impossible).
pub(crate) fn actor_id(
    extractor: &ExtractorIdentity,
    embedder: &EmbedderModel,
    summarizer: &SummarizerIdentity,
) -> Id {
    let key = format!(
        "extract_facts|{}|{}|{}|{}|{}|{}",
        extractor.rule_version,
        extractor.model_family.as_deref().unwrap_or(""),
        extractor.model_version.as_deref().unwrap_or(""),
        embedder.family,
        embedder.version,
        summarizer.rule_version,
    );
    Id::from_content_hash(key.as_bytes())
}

/// A deterministic, replay-stable audit-event id: a content hash over the audit `kind`, the
/// `namespace`, and the `parts` that identify the specific decision. The same logical
/// decision (same episode, same surface / subject / fact) always yields the same id, so
/// re-running an episode dedups its audit events to a no-op (04 §3) — the same guarantee the
/// fact and note ids give. Distinct decisions yield distinct ids.
pub(crate) fn audit_id(kind: &str, namespace: &Namespace, parts: &[&str]) -> Id {
    let mut key = format!("audit|{kind}|{namespace}");
    for part in parts {
        key.push('|');
        key.push_str(part);
    }
    Id::from_content_hash(key.as_bytes())
}

/// The `canonicalize` audit event recording one resolution decision. Its id is keyed on the
/// episode, the resolved surface (text and type), and the entity it resolved to, so a replay
/// of the same episode reproduces it exactly while a different episode resolving the same
/// surface records its own decision.
pub(crate) fn canonicalize_audit(
    actor_id: &Id,
    episode_id: &Id,
    surface: &EntitySurface,
    resolution: &Resolution,
    namespace: &Namespace,
    now: &Timestamp,
) -> AuditEvent {
    let id = audit_id(
        "canonicalize",
        namespace,
        &[
            episode_id.as_str(),
            surface.surface.as_str(),
            surface.entity_type.as_str(),
            resolution.id.as_str(),
        ],
    );
    AuditEvent {
        identity: Identity {
            id,
            ingested_at: now.clone(),
            namespace: namespace.clone(),
            expired_at: None,
        },
        kind: AuditKind::Canonicalize,
        subject_id: resolution.id.clone(),
        actor_id: actor_id.clone(),
        payload: json!({
            "surface": surface.surface,
            "type": surface.entity_type,
            "resolved_to": resolution.id.as_str(),
            "canonical_name": resolution.canonical_name,
            "method": resolution.method.as_str(),
            "is_new": resolution.is_new,
            "confidence": resolution.confidence,
            "candidates": resolution.candidates,
        }),
        signature: String::new(),
        occurred_at: now.clone(),
    }
}

/// The deterministic actor id stamped on a
/// [`SkillInductionPass`](crate::SkillInductionPass)'s audit events: a content hash over the
/// inducer's identity (rule version and model identity). The same induction configuration always
/// attributes its decisions to the same actor across restarts, like the extraction actor id.
pub(crate) fn induction_actor_id(identity: &InducerIdentity) -> Id {
    let key = format!(
        "induce_skills|{}|{}|{}",
        identity.rule_version,
        identity.model_family.as_deref().unwrap_or(""),
        identity.model_version.as_deref().unwrap_or(""),
    );
    Id::from_content_hash(key.as_bytes())
}

/// The `induce_skill` audit event recording one skill induced from a recurring episode (05 §1,
/// M3.T06). Its id is keyed on the source episode, the recurring content, and the inducer rule
/// version, so a replay reproduces it exactly while a rule-version bump records its own decision.
pub(crate) fn induce_skill_audit(
    actor_id: &Id,
    episode: &Episode,
    skill: &Skill,
    recurrence_count: usize,
    rule_version: &str,
    now: &Timestamp,
) -> AuditEvent {
    let namespace = &skill.identity.namespace;
    let id = audit_id(
        "induce_skill",
        namespace,
        &[
            episode.identity.id.as_str(),
            episode.content_hash.as_str(),
            rule_version,
        ],
    );
    AuditEvent {
        identity: Identity {
            id,
            ingested_at: now.clone(),
            namespace: namespace.clone(),
            expired_at: None,
        },
        kind: AuditKind::InduceSkill,
        subject_id: skill.identity.id.clone(),
        actor_id: actor_id.clone(),
        payload: json!({
            "schema_version": 1,
            "episode_id": episode.identity.id.as_str(),
            "content_hash": episode.content_hash.as_str(),
            "recurrence_count": recurrence_count,
            "namespace": namespace.to_string(),
            "induced_skill_id": skill.identity.id.as_str(),
            "induced_skill_name": skill.name,
            "version": skill.version,
            "inducer_rule_version": rule_version,
        }),
        signature: String::new(),
        occurred_at: now.clone(),
    }
}

/// The `skill_deprecate` audit recording that a freshly induced version superseded a prior one
/// (a rule-version bump, 05 §1). Its id is keyed on the new (content-addressed) version's id and
/// the deprecated version, so it is replay-stable like the induce-skill event.
pub(crate) fn induced_deprecate_audit(
    actor_id: &Id,
    prior: &Skill,
    new: &Skill,
    now: &Timestamp,
) -> AuditEvent {
    let namespace = &new.identity.namespace;
    let id = audit_id(
        "skill_deprecate",
        namespace,
        &[new.identity.id.as_str(), &prior.version.to_string()],
    );
    AuditEvent {
        identity: Identity {
            id,
            ingested_at: now.clone(),
            namespace: namespace.clone(),
            expired_at: None,
        },
        kind: AuditKind::SkillDeprecate,
        subject_id: prior.identity.id.clone(),
        actor_id: actor_id.clone(),
        payload: json!({
            "skill_name": new.name,
            "deprecated_version": prior.version,
            "superseded_by_version": new.version,
            "superseded_by_id": new.identity.id.as_str(),
        }),
        signature: String::new(),
        occurred_at: now.clone(),
    }
}

/// The `summarize` audit event recording one cluster's outcome (written or skipped). Its id
/// is keyed on the episode and the cluster's subject, so a replay reproduces it exactly.
pub(crate) fn summarize_audit(
    actor_id: &Id,
    episode_id: &Id,
    cluster: &SummarizationCluster,
    rule_version: &str,
    namespace: &Namespace,
    now: &Timestamp,
    retention: &RetentionOutcome,
) -> AuditEvent {
    let id = audit_id(
        "summarize",
        namespace,
        &[episode_id.as_str(), cluster.subject_id.as_str()],
    );
    AuditEvent {
        identity: Identity {
            id,
            ingested_at: now.clone(),
            namespace: namespace.clone(),
            expired_at: None,
        },
        kind: AuditKind::Summarize,
        subject_id: cluster.subject_id.clone(),
        actor_id: actor_id.clone(),
        payload: json!({
            "outcome": if retention.passed { "written" } else { "skipped_low_retention" },
            "source_fact_count": cluster.facts.len(),
            "entity_count": cluster.entity_names.len(),
            "entity_retention": retention.entity_retention,
            "mean_confidence": retention.mean_confidence,
            "rule_version": rule_version,
        }),
        signature: String::new(),
        occurred_at: now.clone(),
    }
}
