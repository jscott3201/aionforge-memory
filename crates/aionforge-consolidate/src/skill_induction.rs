//! The conservative skill-induction pass (05 §1, M3.T06): off by default.
//!
//! When enabled, this pass derives a private, flagged [`Skill`] from an episode an agent has
//! re-emitted byte-for-byte at least `repetition_threshold` times — repetition as *reuse
//! evidence*, the strongest deterministic signal available with no executor and no LLM (both
//! deferred). It is conservative at every layer:
//!
//! - **Off by default.** [`enabled`](ConsolidationPass::enabled) returns the config flag; a
//!   disabled pass is skipped by the scheduler and excluded from the cursor's `rule_versions`,
//!   so an installation that never opts in carries zero induction footprint.
//! - **Procedural role only.** Only an `Assistant` or `Tool` episode (a produced procedure) is a
//!   candidate; a user utterance or system message never induces.
//! - **Private namespace only.** Induction over a team/global/system episode is structurally
//!   refused, and the induced skill lives in exactly the episode's agent-private namespace —
//!   confinement is a checked precondition, not just an inherited field.
//! - **Reuse-evidence gated.** The exact content must recur at least `repetition_threshold` times
//!   in that namespace, and clear a lexical-structure floor, before anything is induced.
//! - **Never executed, never promoted.** The body is inert data (the episode content verbatim);
//!   the substrate has no executor, and an induced skill is never auto-promoted across a trust
//!   boundary (that is M4's attested path). It earns its reliability from zero like any skill.
//!
//! The actual write rides the atomic consolidation flip: the pass returns the induced skill in
//! the [`PassOutput`], and the store materializes it — content-addressed and idempotent — in the
//! same transaction as the episode's state change.

use std::collections::HashSet;
use std::sync::Arc;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::{InducedSkill, InductionContext, SkillInducer};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::Role;
use aionforge_domain::nodes::procedural::Skill;
use aionforge_store::InducedSkillWrite;

use crate::audit::{induce_skill_audit, induced_deprecate_audit, induction_actor_id};
use crate::config::InductionConfig;
use crate::pass::{ConsolidationPass, PassContext, PassError, PassOutput, PassRun};
use crate::profile::{PassProfile, STAGE_INDUCTION, StageProfile};

/// The importance a freshly induced skill starts at: the neutral mid-point, so it neither
/// dominates nor is buried before it earns a track record.
const INDUCED_IMPORTANCE: f64 = 0.5;

/// The trust a freshly induced skill starts at: neutral. An induced skill has no attester and no
/// outcomes yet, so it sits at the prior, to be earned by use (and demoted by failures).
const INDUCED_TRUST: f64 = 0.5;

/// The conservative skill-induction pass (05 §1, M3.T06). Generic over the [`SkillInducer`] seam;
/// M3 backs it with the deterministic [`RuleInducer`](crate::RuleInducer).
pub struct SkillInductionPass<I: SkillInducer + 'static> {
    inducer: Arc<I>,
    config: InductionConfig,
    actor_id: Id,
}

impl<I: SkillInducer + 'static> SkillInductionPass<I> {
    /// Build the pass over an inducer and its tuning. The audit actor id is derived from the
    /// inducer's identity at construction, so every decision attributes to a stable, replayable
    /// actor.
    #[must_use]
    pub fn new(inducer: Arc<I>, config: InductionConfig) -> Self {
        let actor_id = induction_actor_id(inducer.identity());
        Self {
            inducer,
            config,
            actor_id,
        }
    }

    /// Assemble the induced-skill write: the content-addressed skill, the prior version to
    /// deprecate (if any), and the audit provenance. Version and prior are resolved from the
    /// committed graph; because the scheduler consolidates one episode at a time, that resolution
    /// is stable through to the flip.
    fn build_write(
        &self,
        cx: &PassContext<'_>,
        induced: InducedSkill,
        recurrence_count: usize,
    ) -> Result<InducedSkillWrite, PassError> {
        let episode = cx.episode;
        let namespace = episode.identity.namespace.clone();
        // The inducer's own identity is the authoritative rule version: it keys both the audit
        // actor and the skill id, so the two can never drift.
        let rule_version = self.inducer.identity().rule_version.clone();
        let name = induced_name(&self.config, &namespace, &episode.content_hash);
        let id = induced_id(&namespace, &episode.content_hash, &rule_version);
        let source_hash = ContentHash::of(induced.body.as_bytes());

        let active = cx
            .store
            .active_skill(&name)
            .map_err(|e| PassError::Transient(format!("active_skill probe failed: {e}")))?;
        let next_version = cx
            .store
            .skill_versions(&name)
            .map_err(|e| PassError::Transient(format!("skill_versions probe failed: {e}")))?
            .iter()
            .map(|existing| existing.version)
            .max()
            .map_or(1, |highest| highest.saturating_add(1));

        let skill = Skill {
            identity: Identity {
                id,
                ingested_at: cx.now.clone(),
                namespace,
                expired_at: None,
            },
            stats: Stats {
                importance: INDUCED_IMPORTANCE,
                trust: INDUCED_TRUST,
                last_access: cx.now.clone(),
                access_count_recent: 0,
                referenced_count: 0,
                surprise: 0.0,
                is_pinned: false,
            },
            name,
            version: next_version,
            description: induced.description,
            problem_embedding: None,
            embedder_model: None,
            language: induced.language,
            body: induced.body,
            params: serde_json::Value::Null,
            preconditions: None,
            postconditions: None,
            capabilities: Vec::new(),
            success_count: 0,
            failure_count: 0,
            mean_latency_ms: None,
            source_hash,
            last_success_at: None,
            last_failure_at: None,
            deprecated_at: None,
            induced: true,
        };

        let mut audits = vec![induce_skill_audit(
            &self.actor_id,
            episode,
            &skill,
            recurrence_count,
            &rule_version,
            &cx.now,
        )];
        let deprecate_prior = match &active {
            Some((node, prior)) => {
                audits.push(induced_deprecate_audit(
                    &self.actor_id,
                    prior,
                    &skill,
                    &cx.now,
                ));
                Some(*node)
            }
            None => None,
        };

        Ok(InducedSkillWrite {
            skill,
            deprecate_prior,
            audits,
        })
    }
}

#[async_trait::async_trait]
impl<I: SkillInducer + 'static> ConsolidationPass for SkillInductionPass<I> {
    fn name(&self) -> &'static str {
        "induce_skills"
    }

    fn version(&self) -> u32 {
        1
    }

    fn enabled(&self) -> bool {
        self.config.enabled
    }

    async fn apply(&self, cx: &PassContext<'_>) -> Result<PassRun, PassError> {
        let episode = cx.episode;

        // Gate 1: a produced procedure only — never a user question or a system message.
        // Gate 2: private-namespace precondition — induced skills are confined to the agent's
        // own namespace and never derived from team/global/system memory. Neither gate makes
        // the episode an induction *candidate*; the stage runs but considered nothing.
        if !matches!(episode.role, Role::Assistant | Role::Tool)
            || !episode.identity.namespace.is_private()
        {
            return Ok(induction_run(PassOutput::default(), 0, 0, 0));
        }
        // From here the episode is a candidate the stage considered.
        // Gate 3: the lexical-structure floor — trivial or noise content is not a skill (a guard).
        if !structure_ok(&episode.content, &self.config) {
            tracing::debug!(
                episode = %episode.identity.id,
                "induction: content below the structure floor; not inducing"
            );
            return Ok(induction_run(PassOutput::default(), 1, 0, 1));
        }
        // Gate 4: reuse evidence — the exact content must have recurred enough in this namespace.
        // The probe's window caps the count, so it must be able to reach the threshold; clamp it up
        // so a `repetition_threshold > recurrence_window` misconfiguration cannot silently disable
        // induction by capping the count below a threshold it can never meet.
        let window = self
            .config
            .recurrence_window
            .max(self.config.repetition_threshold);
        let recurrence_count = cx
            .store
            .count_recent_episodes_by_content_hash(
                &episode.identity.namespace,
                &episode.content_hash,
                window,
            )
            .map_err(|e| PassError::Transient(format!("recurrence probe failed: {e}")))?;
        if recurrence_count < self.config.repetition_threshold {
            // Below the reuse-evidence threshold: a candidate the guard rejected.
            return Ok(induction_run(PassOutput::default(), 1, 0, 1));
        }

        // Render via the seam. `None` is a conservative decline; a real error retries next tick.
        let induced = match self
            .inducer
            .induce(episode, &InductionContext { recurrence_count })
            .await
        {
            Ok(Some(induced)) => induced,
            Ok(None) => {
                tracing::debug!(
                    episode = %episode.identity.id,
                    "induction: inducer declined; not inducing"
                );
                // The inducer conservatively declined: a candidate rejected by the seam's guard.
                return Ok(induction_run(PassOutput::default(), 1, 0, 1));
            }
            Err(e) => return Err(PassError::Transient(format!("inducer failed: {e}"))),
        };

        let write = self.build_write(cx, induced, recurrence_count)?;
        let mut out = PassOutput::default();
        out.induced_skills.push(write);
        Ok(induction_run(out, 1, 1, 0))
    }
}

/// Build the induction [`PassRun`]: the artifacts plus the single-stage induction profile.
///
/// The induction stage is reported `enabled` here because [`apply`](ConsolidationPass::apply)
/// only runs when the pass itself is enabled (a disabled induction pass is skipped by the
/// scheduler and contributes no stage line). `candidates` is `1` once the episode clears the
/// role/namespace eligibility filters; `derived`/`rejected` are mutually exclusive (`1`/`0`).
fn induction_run(
    output: PassOutput,
    candidates: u64,
    derived: u64,
    rejected_by_guard: u64,
) -> PassRun {
    PassRun {
        output,
        profile: PassProfile::from_stages(vec![StageProfile::enabled(
            STAGE_INDUCTION,
            candidates,
            derived,
            0,
            0,
            rejected_by_guard,
        )]),
    }
}

/// Whether the content clears the lexical-structure floor: a character-count band and a minimum
/// of distinct whitespace tokens, so a repeated short utterance or one-line log cannot induce.
fn structure_ok(content: &str, config: &InductionConfig) -> bool {
    let chars = content.chars().count();
    if chars < config.min_body_chars || chars > config.max_body_chars {
        return false;
    }
    let distinct: HashSet<&str> = content.split_whitespace().collect();
    distinct.len() >= config.min_distinct_tokens
}

/// The number of hex characters of the name digest kept in an induced skill name: 32 hex chars
/// = 128 bits. The name is the version-lineage key (`active_skill(name)` decides what a new
/// version deprecates), so it must be collision-free across distinct contents in a namespace —
/// 128 bits puts the birthday bound far beyond any realistic induced-skill population.
const NAME_DIGEST_HEX: usize = 32;

/// The deterministic induced-skill name: the configured prefix plus a content hash over the
/// namespace and the recurring content. Stable across rule-version bumps (so versions accumulate
/// under one name), and visibly distinct from authored skill names. Keyed on the episode's
/// `content_hash` (the capture-path hash), so a future transforming inducer would still group its
/// versions by the *source* content, not the rendered body — the intended "one episode-content,
/// one induced skill family" behavior.
fn induced_name(
    config: &InductionConfig,
    namespace: &Namespace,
    content_hash: &ContentHash,
) -> String {
    let digest = ContentHash::of(format!("{namespace}|{}", content_hash.as_str()).as_bytes());
    format!(
        "{}{}",
        config.name_prefix,
        &digest.as_str()[..NAME_DIGEST_HEX]
    )
}

/// The deterministic induced-skill id: a content hash over the namespace, the recurring content,
/// and the inducer rule version. The dedup key that makes a replay a no-op; a rule-version bump
/// changes it, cutting a fresh version under the same name. Like the name, this keys on the
/// episode's `content_hash` (not the rendered body's `source_hash`), so dedup is by source episode
/// content — for the verbatim `RuleInducer` the two are identical, and a future transforming
/// inducer still inducts one skill per recurring episode-content rather than per rendered body.
fn induced_id(namespace: &Namespace, content_hash: &ContentHash, rule_version: &str) -> Id {
    let key = format!(
        "induced|{namespace}|{}|{rule_version}",
        content_hash.as_str()
    );
    Id::from_content_hash(key.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> InductionConfig {
        InductionConfig::default()
    }

    #[test]
    fn structure_floor_rejects_short_or_thin_content() {
        let cfg = config();
        // Below min_body_chars.
        assert!(!structure_ok("too short", &cfg));
        // Enough characters but too few distinct tokens (repeated word).
        assert!(!structure_ok("retry retry retry retry retry retry", &cfg));
        // Clears both floors.
        assert!(structure_ok(
            "run the migration then restart the service and verify health",
            &cfg
        ));
    }

    #[test]
    fn structure_floor_rejects_oversized_content() {
        let mut cfg = config();
        cfg.max_body_chars = 32;
        assert!(!structure_ok(
            "alpha beta gamma delta epsilon zeta eta theta iota kappa",
            &cfg
        ));
    }

    #[test]
    fn name_and_id_are_deterministic_and_id_tracks_rule_version() {
        let cfg = config();
        let ns = Namespace::Agent("alice".to_string());
        let hash = ContentHash::of(b"a recurring procedure body");

        let name_a = induced_name(&cfg, &ns, &hash);
        let name_b = induced_name(&cfg, &ns, &hash);
        assert_eq!(name_a, name_b, "name is reproducible");
        assert!(name_a.starts_with("induced/"));

        let id_a = induced_id(&ns, &hash, "induce-v1");
        let id_b = induced_id(&ns, &hash, "induce-v1");
        assert_eq!(id_a, id_b, "id is reproducible");

        // A rule-version bump changes the id (new version) but NOT the name (same family).
        assert!(name_a.starts_with(&cfg.name_prefix));
        assert_ne!(
            induced_id(&ns, &hash, "induce-v2"),
            id_a,
            "id tracks rule version"
        );
    }

    #[test]
    fn id_separates_namespaces_and_content() {
        let hash = ContentHash::of(b"body");
        let alice = induced_id(&Namespace::Agent("alice".to_string()), &hash, "induce-v1");
        let bob = induced_id(&Namespace::Agent("bob".to_string()), &hash, "induce-v1");
        assert_ne!(alice, bob, "different namespaces → different ids");

        let other = ContentHash::of(b"other body");
        let alice_other = induced_id(&Namespace::Agent("alice".to_string()), &other, "induce-v1");
        assert_ne!(alice, alice_other, "different content → different ids");
    }
}
