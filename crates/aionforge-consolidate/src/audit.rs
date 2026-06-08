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
    EntitySurface, ExtractorIdentity, InducerIdentity, LinkEvolverIdentity, SummarizationCluster,
    SummarizerIdentity,
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

/// The deterministic actor id stamped on the off-cursor [`Distiller`](crate::Distiller)'s audit
/// events: a content hash over the distiller's identity (rule version and the declared model
/// family/version). The same distiller configuration attributes its calls to the same actor
/// across runs, like the extraction and induction actor ids.
pub(crate) fn distill_actor_id(identity: &SummarizerIdentity) -> Id {
    let key = format!(
        "distill|{}|{}|{}",
        identity.rule_version,
        identity.model_family.as_deref().unwrap_or(""),
        identity.model_version.as_deref().unwrap_or(""),
    );
    Id::from_content_hash(key.as_bytes())
}

/// The model-provenance a `distill` audit records for the cross-family guard (07 §T3, M6.T01):
/// the distiller's declared identity, plus the endpoint and seed the call was made with. The
/// endpoint and seed are not derivable from the [`Summarizer`](aionforge_domain::contracts::Summarizer)
/// seam, so the distiller supplies them from its configuration; the API key never appears here.
pub(crate) struct DistillProvenance<'a> {
    /// The distiller's declared identity (model family/version, rule version).
    pub identity: &'a SummarizerIdentity,
    /// The configured completion endpoint (the base URL — not a secret).
    pub endpoint: Option<&'a str>,
    /// The pinned sampling seed the completion was requested with.
    pub seed: Option<i64>,
}

/// The `distill` audit event recording one off-cursor distillation call (M3.T08). Unlike the
/// cursor `summarize` audit, its id is keyed on the cluster's full source-fact set (like the
/// distilled note's id) **and the outcome**, so a replay of the same cluster-state dedups to a
/// no-op while a genuinely different outcome (the model recovered and a previously declined
/// cluster now writes) records its own call rather than overwriting the old verdict.
#[allow(clippy::too_many_arguments)]
pub(crate) fn distill_audit(
    actor_id: &Id,
    cluster: &SummarizationCluster,
    provenance: &DistillProvenance<'_>,
    outcome: &str,
    retention: Option<&RetentionOutcome>,
    note_id: Option<&Id>,
    namespace: &Namespace,
    now: &Timestamp,
) -> AuditEvent {
    let mut fact_ids: Vec<&str> = cluster
        .facts
        .iter()
        .map(|f| f.identity.id.as_str())
        .collect();
    fact_ids.sort_unstable();
    let facts_key = fact_ids.join(",");
    let id = audit_id(
        "distill",
        namespace,
        &[
            cluster.subject_id.as_str(),
            &facts_key,
            &provenance.identity.rule_version,
            outcome,
        ],
    );
    AuditEvent {
        identity: Identity {
            id,
            ingested_at: now.clone(),
            namespace: namespace.clone(),
            expired_at: None,
        },
        kind: AuditKind::Distill,
        subject_id: cluster.subject_id.clone(),
        actor_id: actor_id.clone(),
        payload: json!({
            "outcome": outcome,
            "model_family": provenance.identity.model_family,
            "model_version": provenance.identity.model_version,
            "rule_version": provenance.identity.rule_version,
            "endpoint": provenance.endpoint,
            "seed": provenance.seed,
            "source_fact_count": cluster.facts.len(),
            "entity_count": cluster.entity_names.len(),
            "entity_retention": retention.map(|r| r.entity_retention),
            "mean_confidence": retention.map(|r| r.mean_confidence),
            "note_id": note_id.map(Id::as_str),
        }),
        signature: String::new(),
        occurred_at: now.clone(),
    }
}

/// The deterministic actor id stamped on the off-cursor
/// [`LinkEvolvePass`](crate::LinkEvolvePass)'s audit events: a content hash over the evolver's
/// identity (rule version and the declared model family/version). The same evolver configuration
/// attributes its calls to the same actor across runs, like the distillation actor id.
pub(crate) fn link_evolve_actor_id(identity: &LinkEvolverIdentity) -> Id {
    let key = format!(
        "link_evolve|{}|{}|{}",
        identity.rule_version,
        identity.model_family.as_deref().unwrap_or(""),
        identity.model_version.as_deref().unwrap_or(""),
    );
    Id::from_content_hash(key.as_bytes())
}

/// The model-provenance a `link_evolve` audit records for the cross-family guard (07 §T3, M6.T01):
/// the evolver's declared identity, plus the endpoint and seed the call was made with. As with
/// distillation these are not derivable from the
/// [`LinkEvolver`](aionforge_domain::contracts::LinkEvolver) seam, so the driver supplies them from
/// its configuration; the API key never appears here.
pub(crate) struct LinkEvolveProvenance<'a> {
    /// The evolver's declared identity (model family/version, rule version).
    pub identity: &'a LinkEvolverIdentity,
    /// The configured completion endpoint (the base URL — not a secret).
    pub endpoint: Option<&'a str>,
    /// The pinned sampling seed the completion was requested with.
    pub seed: Option<i64>,
}

/// One create-or-revise decision the link-evolution driver made for a source note, recorded in the
/// `link_evolve` audit payload so a forensic query can see exactly which relationships a call drew.
pub(crate) struct LinkDecision {
    /// `"created"` for a new link, `"revised"` for a closed-and-reopened relabeling.
    pub action: &'static str,
    /// The target note id the relationship points to.
    pub target: String,
    /// The relationship label written (from the closed vocabulary).
    pub label: String,
    /// The evolver's confidence in the relationship.
    pub confidence: f64,
}

/// The `link_evolve` audit event recording one off-cursor link-evolution call against a source note
/// (M3.T09). Its id is keyed on the source note, the evolver rule version, the outcome, and the
/// sorted decision set, so a replay of the deterministic rule evolver dedups to a no-op while a
/// genuinely different outcome records its own call. Wired `AuditEvent -AUDIT-> Note` to the source
/// note by [`Store::materialize_link_edges`](aionforge_store::Store::materialize_link_edges).
pub(crate) fn link_evolve_audit(
    actor_id: &Id,
    source_id: &Id,
    provenance: &LinkEvolveProvenance<'_>,
    outcome: &str,
    decisions: &[LinkDecision],
    namespace: &Namespace,
    now: &Timestamp,
) -> AuditEvent {
    let mut decision_keys: Vec<String> = decisions
        .iter()
        .map(|d| format!("{}:{}:{}", d.action, d.target, d.label))
        .collect();
    decision_keys.sort_unstable();
    let decisions_key = decision_keys.join(",");
    let id = audit_id(
        "link_evolve",
        namespace,
        &[
            source_id.as_str(),
            &provenance.identity.rule_version,
            outcome,
            &decisions_key,
        ],
    );
    let links: Vec<serde_json::Value> = decisions
        .iter()
        .map(|d| {
            json!({
                "action": d.action,
                "target": d.target,
                "label": d.label,
                "confidence": d.confidence,
            })
        })
        .collect();
    AuditEvent {
        identity: Identity {
            id,
            ingested_at: now.clone(),
            namespace: namespace.clone(),
            expired_at: None,
        },
        kind: AuditKind::LinkEvolve,
        subject_id: source_id.clone(),
        actor_id: actor_id.clone(),
        payload: json!({
            "outcome": outcome,
            "model_family": provenance.identity.model_family,
            "model_version": provenance.identity.model_version,
            "rule_version": provenance.identity.rule_version,
            "endpoint": provenance.endpoint,
            "seed": provenance.seed,
            "links": links,
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
