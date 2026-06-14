//! A deterministic, rule-based skill inducer (05 §1, M3.T06).
//!
//! This is the shipped inducer — induction is rule-based, testable with no network, and the
//! induced skill id stays reproducible. It is deliberately transparent: the induced skill's body
//! is the recurring
//! episode's content **verbatim** — there is no summarization or extraction step, so nothing
//! about the body is non-deterministic, and an operator auditing an induced skill sees exactly
//! the procedure the agent re-emitted. The conservative gates (reuse threshold, procedural role,
//! private namespace, lexical structure) live in the pass ([`crate::skill_induction`]); this
//! seam only renders the candidate, mirroring how [`crate::rule_summarizer`] only renders prose.
//!
//! Its [`Error`](aionforge_domain::contracts::SkillInducer::Error) is
//! [`Infallible`]: a pure rule inducer cannot fail, and the type
//! statically forbids a network- or model-backed default from ever masquerading as the M3
//! inducer.

use std::convert::Infallible;
use std::future::Future;

use aionforge_domain::contracts::{InducedSkill, InducerIdentity, InductionContext, SkillInducer};
use aionforge_domain::nodes::episodic::Episode;

/// The most characters of the first content line the description carries (a recall label, not
/// the whole body — the body is the recall floor's full text).
const DESCRIPTION_CAP: usize = 200;

/// The language tag stamped on a rule-induced skill: the substrate cannot deterministically
/// infer a body's language, so it records the honest "unknown / plain text" tag rather than
/// guessing one a downstream executor might trust.
const RULE_INDUCED_LANGUAGE: &str = "text";

/// A deterministic [`SkillInducer`] that renders a recurring episode into a verbatim-body skill.
#[derive(Debug, Clone)]
pub struct RuleInducer {
    identity: InducerIdentity,
}

impl RuleInducer {
    /// Build an inducer with an explicit rule-set version.
    #[must_use]
    pub fn new(rule_version: impl Into<String>) -> Self {
        Self {
            identity: InducerIdentity {
                model_family: None,
                model_version: None,
                rule_version: rule_version.into(),
            },
        }
    }

    /// Build an inducer with the default rule set (`induce-v1`).
    #[must_use]
    pub fn with_default_rules() -> Self {
        Self::new("induce-v1")
    }

    /// Render synchronously — the whole of the work; the async seam just wraps this.
    fn induce_sync(&self, episode: &Episode) -> Option<InducedSkill> {
        // The pass has already cleared the structure floor, but an empty body is never a skill.
        if episode.content.trim().is_empty() {
            return None;
        }
        Some(InducedSkill {
            description: describe(&episode.content),
            language: RULE_INDUCED_LANGUAGE.to_string(),
            body: episode.content.clone(),
        })
    }
}

impl SkillInducer for RuleInducer {
    type Error = Infallible;

    fn induce(
        &self,
        episode: &Episode,
        _cx: &InductionContext,
    ) -> impl Future<Output = Result<Option<InducedSkill>, Self::Error>> + Send {
        let out = self.induce_sync(episode);
        async move { Ok(out) }
    }

    fn identity(&self) -> &InducerIdentity {
        &self.identity
    }
}

/// A deterministic, recall-friendly description: the first non-empty line of the body, trimmed
/// and capped. Deterministic so the induced skill's BM25 surface is reproducible.
fn describe(content: &str) -> String {
    let first = content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let capped: String = first.chars().take(DESCRIPTION_CAP).collect();
    if capped.is_empty() {
        "induced procedure".to_string()
    } else {
        capped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionforge_domain::blocks::{Identity, Stats};
    use aionforge_domain::ids::{ContentHash, Id};
    use aionforge_domain::namespace::Namespace;
    use aionforge_domain::nodes::episodic::{ConsolidationState, Role};
    use aionforge_domain::time::Timestamp;

    fn ts() -> Timestamp {
        "2026-06-07T12:00:00-05:00[America/Chicago]"
            .parse()
            .expect("valid timestamp")
    }

    fn episode(content: &str) -> Episode {
        Episode {
            identity: Identity {
                id: Id::generate(),
                ingested_at: ts(),
                namespace: Namespace::Agent("alice".to_string()),
                expired_at: None,
            },
            stats: Stats {
                importance: 0.5,
                trust: 0.5,
                last_access: ts(),
                access_count_recent: 0,
                referenced_count: 0,
                surprise: 0.0,
                is_pinned: false,
            },
            content: content.to_string(),
            role: Role::Assistant,
            captured_at: ts(),
            agent_id: Id::generate(),
            session_id: None,
            content_hash: ContentHash::of(content.as_bytes()),
            embedding: None,
            embedder_model: None,
            consolidation_state: ConsolidationState::Raw,
            origin: None,
        }
    }

    #[test]
    fn body_is_verbatim_and_description_is_first_line() {
        let inducer = RuleInducer::with_default_rules();
        let ep = episode("run the migration\nthen restart the service\nverify health");
        let out = inducer.induce_sync(&ep).expect("induces");
        assert_eq!(out.body, ep.content, "body is the content verbatim");
        assert_eq!(out.description, "run the migration");
        assert_eq!(out.language, "text");
    }

    #[test]
    fn rendering_is_deterministic() {
        let inducer = RuleInducer::with_default_rules();
        let ep = episode("alpha beta gamma delta epsilon");
        assert_eq!(inducer.induce_sync(&ep), inducer.induce_sync(&ep));
    }

    #[test]
    fn empty_body_declines() {
        let inducer = RuleInducer::with_default_rules();
        assert!(inducer.induce_sync(&episode("   \n  \t ")).is_none());
    }

    #[test]
    fn description_skips_leading_blank_lines_and_caps_length() {
        let inducer = RuleInducer::with_default_rules();
        let long = "x".repeat(DESCRIPTION_CAP + 50);
        let ep = episode(&format!("\n\n   {long}"));
        let out = inducer.induce_sync(&ep).expect("induces");
        assert_eq!(out.description.chars().count(), DESCRIPTION_CAP);
    }

    #[test]
    fn identity_carries_rule_version_and_no_model() {
        let inducer = RuleInducer::new("induce-v9");
        assert_eq!(inducer.identity().rule_version, "induce-v9");
        assert!(inducer.identity().model_family.is_none());
    }
}
