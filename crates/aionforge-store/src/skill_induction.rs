//! Atomic materialization of consolidation-induced skills (05 §1, M3.T06).
//!
//! A skill-induction pass reads a snapshot and returns an [`InducedSkillWrite`] in the
//! consolidation artifacts; this module writes it into the **same** flip transaction as the
//! episode's state change, so an induced skill and the flip commit together or not at all — a
//! crash never leaves an orphan skill and a re-run never double-inducts. It mirrors the summary
//! note materializer ([`crate::note`]): an in-mutator helper with content-addressed,
//! skip-on-replay dedup.
//!
//! Idempotency is by the skill's content-addressed id. The pass derives the id deterministically
//! (namespace + content hash + inducer rule version), so re-consolidating the same episode
//! reconstructs the same id; [`materialize_induced_skills`] probes that id against the committed
//! graph first and, if the skill already exists, **skips the whole write** — node, deprecation,
//! audits, and lineage — a pure no-op exactly like a replayed note.
//!
//! Two safety properties beyond idempotency: the writer reuses [`write_skill_into`] so an induced
//! skill is persisted by the same deprecate-never-delete logic as an authored one (a rule-version
//! bump cuts a new version and deprecates the prior), and a fail-closed guard rejects a batch that
//! carries two different bodies under one name rather than thrashing the version history.

use std::collections::HashMap;

use aionforge_domain::edges::DerivedFrom;
use aionforge_domain::ids::ContentHash;
use aionforge_domain::nodes::forensic::AuditEvent;
use aionforge_domain::nodes::procedural::Skill;
use aionforge_domain::time::Timestamp;
use selene_core::NodeId;
use selene_graph::Mutator;

use crate::error::StoreError;
use crate::materialize::{derived_from_props, ensure_edge};
use crate::skill::{skill_node_id_in, write_skill_into};

/// An induced skill to materialize, with the prior version it deprecates and its provenance
/// (05 §1, M3.T06).
///
/// The pass resolves `deprecate_prior` (the active version of this name, if any) and builds the
/// full `audits` set (an `InduceSkill` event, plus a `SkillDeprecate` when a prior version is
/// superseded), so this layer stays purely mechanical — it applies what the pass decided and
/// re-derives no policy. Its `skill.identity.id` is content-addressed, the dedup key that makes a
/// replay a no-op.
#[derive(Debug, Clone)]
pub struct InducedSkillWrite {
    /// The induced skill node to write (`induced = true`, private namespace, content-addressed id).
    pub skill: Skill,
    /// The prior active version of this name to deprecate, if one exists (deprecate-never-delete).
    pub deprecate_prior: Option<NodeId>,
    /// The provenance to wire `AuditEvent -AUDIT-> Skill` (an `InduceSkill`, plus `SkillDeprecate`
    /// on a version bump).
    pub audits: Vec<AuditEvent>,
}

/// Write each induced skill into the open flip transaction, idempotently (05 §1, M3.T06).
///
/// For each write: if a skill with this content-addressed id already exists in the committed
/// graph, skip it entirely (the replay no-op); otherwise create the version via
/// [`write_skill_into`] (which also deprecates `deprecate_prior` and wires the audit edges) and
/// link `Skill -DERIVED_FROM-> Episode` so an induced skill's source episode is graph-traversable
/// provenance. A fail-closed guard rejects a batch that asserts two different bodies under one
/// name (an inducer invariant violation) rather than silently version-thrashing.
///
/// # Errors
/// Returns [`StoreError::Invariant`] on the dup-name conflict, or [`StoreError`] if a probe or a
/// mutation fails.
pub(crate) fn materialize_induced_skills(
    mutator: &mut Mutator<'_, '_>,
    writes: &[InducedSkillWrite],
    episode_node_id: NodeId,
    now: &Timestamp,
) -> Result<(), StoreError> {
    // Fail closed: one name must map to one body within a single episode's output. (One episode
    // induces at most one skill today, so this only guards a future multi-induction pass.)
    let mut body_of_name: HashMap<&str, &ContentHash> = HashMap::new();
    for write in writes {
        if let Some(prior) = body_of_name.insert(&write.skill.name, &write.skill.source_hash)
            && prior != &write.skill.source_hash
        {
            return Err(StoreError::invariant(format!(
                "induction asserted two bodies for skill name `{}`",
                write.skill.name
            )));
        }
    }

    for write in writes {
        // Idempotent replay: the content-addressed id already exists → skip the whole write
        // (node, deprecation, audits, lineage), exactly like a replayed note.
        if skill_node_id_in(mutator.read(), &write.skill.identity.id)?.is_some() {
            continue;
        }
        let skill_node =
            write_skill_into(mutator, &write.skill, write.deprecate_prior, &write.audits)?;
        // Provenance: the induced skill derives from the episode it was induced from.
        ensure_edge(
            mutator,
            DerivedFrom::LABEL,
            skill_node,
            episode_node_id,
            derived_from_props(now)?,
        )?;
    }
    Ok(())
}
