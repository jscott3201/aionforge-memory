//! A deterministic, rule-based link evolver (M3.T09): the [`LinkEvolver`] seam's hermetic default.
//!
//! Where the model-backed [`LLMLinkEvolver`](crate::LLMLinkEvolver) judges *which kind* of
//! relationship holds, the rule evolver draws the one relationship a pure-vector method can infer:
//! `related_to`, to every candidate it is offered, with the source-to-candidate embedding cosine as
//! the confidence. It is [`Infallible`](std::convert::Infallible) and fully deterministic, so the
//! off-cursor [`LinkEvolvePass`](crate::LinkEvolvePass) is testable with no network and the rule
//! tier is always present beneath the optional LLM tier (the layered-determinism doctrine, 04
//! §*Canonical vs. distilled*). The candidate set, the confidence floor, and the cascade guard are
//! the driver's policy; this seam only scores proximity.

use std::convert::Infallible;
use std::future::Future;

use aionforge_domain::contracts::{EvolvedLink, LinkEvolver, LinkEvolverIdentity};
use aionforge_domain::nodes::associative::Note;

use crate::link_evolution::{RELATED_TO, cosine};

/// The rule-set version stamped on every rule-evolved link's provenance, and the actor key for the
/// deterministic evolver.
pub const RULE_LINK_EVOLVE_VERSION: &str = "rule-link-evolve-v1";

/// A deterministic [`LinkEvolver`] that proposes `related_to` to each candidate by embedding cosine.
#[derive(Debug, Clone)]
pub struct RuleLinkEvolver {
    identity: LinkEvolverIdentity,
}

impl RuleLinkEvolver {
    /// Build an evolver with an explicit rule-set version.
    #[must_use]
    pub fn new(rule_version: impl Into<String>) -> Self {
        Self {
            identity: LinkEvolverIdentity {
                model_family: None,
                model_version: None,
                rule_version: rule_version.into(),
            },
        }
    }

    /// Build an evolver with the default rule set (`rule-link-evolve-v1`).
    #[must_use]
    pub fn with_default_rules() -> Self {
        Self::new(RULE_LINK_EVOLVE_VERSION)
    }

    /// Score each candidate by source-to-candidate cosine and propose `related_to`. A source or
    /// candidate without an embedding is skipped (no vector, no proximity); the driver's confidence
    /// floor drops the dissimilar tail.
    fn evolve_sync(&self, source: &Note, candidates: &[Note]) -> Vec<EvolvedLink> {
        let Some(source_vec) = source.embedding.as_ref() else {
            return Vec::new();
        };
        let mut links = Vec::new();
        for candidate in candidates {
            let Some(candidate_vec) = candidate.embedding.as_ref() else {
                continue;
            };
            links.push(EvolvedLink {
                target_id: candidate.identity.id.clone(),
                relationship_label: RELATED_TO.to_string(),
                confidence: cosine(source_vec.as_slice(), candidate_vec.as_slice()),
            });
        }
        links
    }
}

impl LinkEvolver for RuleLinkEvolver {
    type Error = Infallible;

    fn evolve(
        &self,
        source: &Note,
        candidates: &[Note],
    ) -> impl Future<Output = Result<Option<Vec<EvolvedLink>>, Self::Error>> + Send {
        let links = self.evolve_sync(source, candidates);
        async move { Ok(Some(links)) }
    }

    fn identity(&self) -> &LinkEvolverIdentity {
        &self.identity
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionforge_domain::blocks::{Identity, Stats};
    use aionforge_domain::embedding::Embedding;
    use aionforge_domain::ids::Id;
    use aionforge_domain::namespace::Namespace;

    fn ts() -> aionforge_domain::time::Timestamp {
        "2026-06-06T09:00:00Z[UTC]".parse().expect("valid ts")
    }

    fn note(seed: &[u8], embedding: Option<Vec<f32>>) -> Note {
        Note {
            identity: Identity {
                id: Id::from_content_hash(seed),
                ingested_at: ts(),
                namespace: Namespace::Agent("t".to_string()),
                expired_at: None,
            },
            stats: Stats {
                importance: 0.5,
                trust: 0.8,
                last_access: ts(),
                access_count_recent: 0,
                referenced_count: 0,
                surprise: 0.0,
                is_pinned: false,
            },
            content: format!("note {}", String::from_utf8_lossy(seed)),
            context: None,
            keywords: Vec::new(),
            embedding: embedding.map(|v| Embedding::new(v).expect("valid embedding")),
            embedder_model: None,
            derived_from_episode: None,
        }
    }

    #[tokio::test]
    async fn proposes_related_to_each_candidate_with_cosine_confidence() {
        let evolver = RuleLinkEvolver::with_default_rules();
        let source = note(b"s", Some(vec![1.0, 0.0, 0.0, 0.0]));
        let near = note(b"near", Some(vec![1.0, 0.0, 0.0, 0.0])); // identical → cosine 1.0
        let orth = note(b"orth", Some(vec![0.0, 1.0, 0.0, 0.0])); // orthogonal → cosine 0.0
        let links = evolver
            .evolve(&source, &[near.clone(), orth.clone()])
            .await
            .expect("infallible")
            .expect("ran");
        assert_eq!(links.len(), 2, "one proposal per candidate");
        assert!(links.iter().all(|l| l.relationship_label == RELATED_TO));
        let near_link = links
            .iter()
            .find(|l| l.target_id == near.identity.id)
            .expect("near");
        let orth_link = links
            .iter()
            .find(|l| l.target_id == orth.identity.id)
            .expect("orth");
        assert!((near_link.confidence - 1.0).abs() < 1e-9, "identical → 1.0");
        assert!(orth_link.confidence.abs() < 1e-9, "orthogonal → 0.0");
    }

    #[tokio::test]
    async fn is_deterministic_across_runs() {
        let evolver = RuleLinkEvolver::with_default_rules();
        let source = note(b"s", Some(vec![0.2, 0.4, 0.1, 0.9]));
        let cands = vec![
            note(b"a", Some(vec![0.1, 0.5, 0.2, 0.8])),
            note(b"b", Some(vec![0.9, 0.1, 0.0, 0.1])),
        ];
        let first = evolver
            .evolve(&source, &cands)
            .await
            .expect("ok")
            .expect("ran");
        let second = evolver
            .evolve(&source, &cands)
            .await
            .expect("ok")
            .expect("ran");
        assert_eq!(first, second, "same input → same proposals");
    }

    #[tokio::test]
    async fn a_source_without_an_embedding_proposes_nothing() {
        let evolver = RuleLinkEvolver::with_default_rules();
        let source = note(b"s", None);
        let cand = note(b"c", Some(vec![1.0, 0.0, 0.0, 0.0]));
        let links = evolver
            .evolve(&source, &[cand])
            .await
            .expect("ok")
            .expect("ran");
        assert!(links.is_empty(), "no source vector, no proximity");
    }

    #[test]
    fn identity_marks_a_pure_rule_evolver() {
        let evolver = RuleLinkEvolver::with_default_rules();
        assert!(evolver.identity().model_family.is_none());
        assert_eq!(evolver.identity().rule_version, RULE_LINK_EVOLVE_VERSION);
    }
}
