//! The procedural-memory engine: versioned saves, outcome recording, and reliability-weighted
//! retrieval over the L0 skill surface (05; M3.T04).
//!
//! [`ProceduralMemoryService`] holds the policies the L0 [`Store`] leaves to its caller —
//! version assignment, deprecation, audit construction, change detection, and the
//! reliability-weighted ranking — and is generic over the [`Embedder`] seam so it never names a
//! concrete embedding client. Every store mutation rides the L0 single-write funnel; this layer
//! only decides *what* to write.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use aionforge_domain::blocks::Identity;
use aionforge_domain::contracts::{Embedder, ProceduralMemory};
use aionforge_domain::embedding::Embedding;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::procedural::{RankedSkill, Skill};
use aionforge_domain::time::Timestamp;
use aionforge_store::{NodeId, SearchKind, Store};

use crate::clock::{Clock, SystemClock};
use crate::config::ProceduralConfig;
use crate::error::ProceduralError;
use crate::ranking::{self, WeightedRanking, rrf};

/// The procedural-memory engine over a shared [`Store`], an [`Embedder`], and an injected clock.
///
/// Scoped to one actor: the `actor_id` is recorded on every audit event the layer emits, so the
/// version history names who saved each version.
#[derive(Debug, Clone)]
pub struct ProceduralMemoryService<E, C = SystemClock> {
    store: Arc<Store>,
    embedder: E,
    actor_id: Id,
    config: ProceduralConfig,
    clock: C,
}

impl<E> ProceduralMemoryService<E, SystemClock>
where
    E: Embedder,
{
    /// Build a service over a shared store and embedder, stamping bookkeeping times from the
    /// system clock.
    #[must_use]
    pub fn new(store: Arc<Store>, embedder: E, actor_id: Id, config: ProceduralConfig) -> Self {
        Self::with_clock(store, embedder, actor_id, config, SystemClock)
    }
}

impl<E, C> ProceduralMemoryService<E, C>
where
    E: Embedder,
    C: Clock,
{
    /// Build a service with an explicit clock (tests inject a fixed clock for determinism).
    #[must_use]
    pub fn with_clock(
        store: Arc<Store>,
        embedder: E,
        actor_id: Id,
        config: ProceduralConfig,
        clock: C,
    ) -> Self {
        Self {
            store,
            embedder,
            actor_id,
            config,
            clock,
        }
    }

    /// Save a skill, assigning its version and constructing the audit trail.
    async fn run_save(&self, mut skill: Skill) -> Result<Id, ProceduralError> {
        // Make the skill retrievable by vector: compute its problem embedding from the
        // description when the caller supplied none. Fail closed (see `ProceduralError::Embed`).
        if skill.problem_embedding.is_none() {
            skill.problem_embedding = Some(self.embed_problem(&skill.description).await?);
            skill.embedder_model = Some(self.embedder.model().clone());
        }

        let now = self.clock.now();
        let active = self.store.active_skill(&skill.name)?;

        // Change detection: an identical procedure re-saved against the active version is a
        // no-op, so re-registering an unchanged skill never churns the version history.
        if let Some((_, current)) = &active
            && is_unchanged(current, &skill)
        {
            return Ok(current.identity.id.clone());
        }

        // The next version is one past the highest ever recorded for this name — robust even if
        // a name somehow has no active version — and the prior active version, if any, is the
        // one this save deprecates.
        let next_version = self
            .store
            .skill_versions(&skill.name)?
            .iter()
            .map(|existing| existing.version)
            .max()
            .map_or(1, |highest| highest.saturating_add(1));

        // Stamp the version-node identity and reset reliability: a changed body is a different
        // procedure whose success record is earned from zero, never inherited.
        let skill_id = Id::generate();
        skill.identity.id = skill_id.clone();
        skill.identity.ingested_at = now.clone();
        skill.identity.expired_at = None;
        skill.stats.last_access = now.clone();
        skill.version = next_version;
        skill.success_count = 0;
        skill.failure_count = 0;
        skill.mean_latency_ms = None;
        skill.last_success_at = None;
        skill.last_failure_at = None;
        skill.deprecated_at = None;

        let prior = active.as_ref().map(|(_, current)| current);
        let deprecate_prior = active.as_ref().map(|(node, _)| *node);
        let audits = self.build_save_audits(&skill, prior, &now);

        self.store.save_skill(&skill, deprecate_prior, &audits)?;
        Ok(skill_id)
    }

    /// Record an outcome against a specific skill version, bridging domain id to node id.
    async fn run_record(&self, skill_id: Id, success: bool) -> Result<(), ProceduralError> {
        let node = self
            .store
            .skill_node_by_id(&skill_id)?
            .ok_or_else(|| ProceduralError::NotFound(skill_id.to_string()))?;
        let now = self.clock.now();
        self.store.record_skill_outcome(node, success, &now)?;
        Ok(())
    }

    /// Retrieve active skills by problem match, reliability-weighted, best first.
    async fn run_retrieve(
        &self,
        problem: String,
        k: usize,
    ) -> Result<Vec<RankedSkill>, ProceduralError> {
        if k == 0 {
            return Ok(Vec::new());
        }
        // Over-fetch each signal so reliability re-ranking can lift a proven-but-slightly-less-
        // similar skill above an unproven top match.
        let pool = k.saturating_mul(self.config.candidate_multiplier).max(k);

        // Vector signal over the problem embedding. Resilient: a down embedder falls back to the
        // lexical signal alone — BM25 over the description is the recall floor (mirrors M2.T08).
        let vector_nodes: Vec<NodeId> = match self.embed_problem(&problem).await {
            Ok(query) => self
                .store
                .vector_search_ann(SearchKind::Skill, &query, pool)?
                .into_iter()
                .map(|hit| hit.node)
                .collect(),
            Err(_) => Vec::new(),
        };

        // Lexical signal over the description.
        let text_nodes: Vec<NodeId> = self
            .store
            .text_search(SearchKind::Skill, &problem, pool)?
            .into_iter()
            .map(|hit| hit.node)
            .collect();

        let fused = rrf(
            &[
                WeightedRanking {
                    weight: self.config.vector_weight,
                    nodes: &vector_nodes,
                },
                WeightedRanking {
                    weight: self.config.text_weight,
                    nodes: &text_nodes,
                },
            ],
            self.config.rrf_k,
        );

        // Resolve to live, active skills, weight problem match by reliability, then rank.
        let mut ranked: Vec<RankedSkill> = Vec::new();
        for (node, similarity) in fused {
            let Some(skill) = self.store.skill_by_node_id(node)? else {
                continue;
            };
            // Deprecated or soft-forgotten versions are history, never retrieval candidates.
            if skill.deprecated_at.is_some() || skill.identity.expired_at.is_some() {
                continue;
            }
            let reliability = ranking::reliability(
                self.config.prior_alpha,
                self.config.prior_beta,
                skill.success_count,
                skill.failure_count,
            );
            let score = similarity * reliability;
            ranked.push(RankedSkill {
                skill,
                similarity,
                reliability,
                score,
            });
        }
        // Final score descending; skill id ascending breaks ties for a deterministic order.
        ranked.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then(a.skill.identity.id.cmp(&b.skill.identity.id))
        });
        ranked.truncate(k);
        Ok(ranked)
    }

    /// Embed one problem string, normalizing for the cosine write path; map any embedder failure
    /// into [`ProceduralError::Embed`].
    async fn embed_problem(&self, text: &str) -> Result<Embedding, ProceduralError> {
        let inputs = [text.to_string()];
        let vectors = self
            .embedder
            .embed(&inputs)
            .await
            .map_err(|error| ProceduralError::Embed(error.to_string()))?;
        let vector = vectors
            .into_iter()
            .next()
            .ok_or_else(|| ProceduralError::Embed("the embedder returned no vector".to_string()))?;
        Ok(vector.normalized())
    }

    /// The audit set for a save: always a `SkillSave`, plus a `SkillDeprecate` and a
    /// `SkillVersionDiff` (the audited capability diff) when a prior version is superseded.
    fn build_save_audits(
        &self,
        new: &Skill,
        prior: Option<&Skill>,
        now: &Timestamp,
    ) -> Vec<AuditEvent> {
        let mut audits = Vec::with_capacity(3);
        audits.push(self.audit(
            AuditKind::SkillSave,
            &new.identity.id,
            now,
            serde_json::json!({
                "name": new.name,
                "version": new.version,
                "source_hash": new.source_hash.as_str(),
                "induced": new.induced,
            }),
        ));
        if let Some(prior) = prior {
            audits.push(self.audit(
                AuditKind::SkillDeprecate,
                &prior.identity.id,
                now,
                serde_json::json!({
                    "name": prior.name,
                    "deprecated_version": prior.version,
                    "superseded_by_version": new.version,
                }),
            ));
            let (added, removed) = capability_diff(&prior.capabilities, &new.capabilities);
            audits.push(self.audit(
                AuditKind::SkillVersionDiff,
                &new.identity.id,
                now,
                serde_json::json!({
                    "name": new.name,
                    "from_version": prior.version,
                    "to_version": new.version,
                    "capabilities_added": added,
                    "capabilities_removed": removed,
                    "body_changed": prior.source_hash != new.source_hash,
                }),
            ));
        }
        audits
    }

    /// Build one unsigned audit event in the system namespace (audits live there, 02 §11).
    fn audit(
        &self,
        kind: AuditKind,
        subject: &Id,
        now: &Timestamp,
        payload: serde_json::Value,
    ) -> AuditEvent {
        AuditEvent {
            identity: Identity {
                id: Id::generate(),
                ingested_at: now.clone(),
                namespace: Namespace::System,
                expired_at: None,
            },
            kind,
            subject_id: subject.clone(),
            actor_id: self.actor_id.clone(),
            payload,
            signature: String::new(),
            occurred_at: now.clone(),
        }
    }
}

impl<E, C> ProceduralMemory for ProceduralMemoryService<E, C>
where
    E: Embedder,
    C: Clock,
{
    type Error = ProceduralError;

    fn save_skill(&self, skill: Skill) -> impl Future<Output = Result<Id, Self::Error>> + Send {
        self.run_save(skill)
    }

    fn record_outcome(
        &self,
        skill_id: Id,
        success: bool,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.run_record(skill_id, success)
    }

    fn retrieve_skills(
        &self,
        problem: String,
        k: usize,
    ) -> impl Future<Output = Result<Vec<RankedSkill>, Self::Error>> + Send {
        self.run_retrieve(problem, k)
    }
}

/// Whether `incoming` is the same procedure as the active `current` version, so a re-save is a
/// no-op rather than a new version.
///
/// `source_hash` is the body's blake3 digest (the documented change key, 02 §4.4); the
/// capabilities (compared as a set, order-independent), params, and pre/post-conditions are the
/// rest of the per-version frozen contract surface, and `language` governs how the body runs. A
/// change to any of those cuts a new version — including the capabilities frozen per version.
/// The description is excluded: it is a recall surface, not part of the procedure, so editing it
/// alone does not churn the history.
fn is_unchanged(current: &Skill, incoming: &Skill) -> bool {
    current.source_hash == incoming.source_hash
        && current.language == incoming.language
        && current.params == incoming.params
        && current.preconditions == incoming.preconditions
        && current.postconditions == incoming.postconditions
        && capability_set(&current.capabilities) == capability_set(&incoming.capabilities)
}

/// The capabilities as a set of slices, for order-independent comparison and diffing.
fn capability_set(capabilities: &[String]) -> BTreeSet<&str> {
    capabilities.iter().map(String::as_str).collect()
}

/// Capabilities added in `new` and removed from `prior`, each sorted for a stable audit payload.
fn capability_diff(prior: &[String], new: &[String]) -> (Vec<String>, Vec<String>) {
    let prior_set = capability_set(prior);
    let new_set = capability_set(new);
    let added = new_set
        .difference(&prior_set)
        .map(|capability| (*capability).to_string())
        .collect();
    let removed = prior_set
        .difference(&new_set)
        .map(|capability| (*capability).to_string())
        .collect();
    (added, removed)
}

#[cfg(test)]
mod tests {
    use super::capability_diff;

    #[test]
    fn capability_diff_is_sorted_and_set_based() {
        let prior = ["fs.read".to_string(), "net.get".to_string()];
        // Reordered, one dropped (`net.get`), two added (`fs.write`, `db.read`).
        let new = [
            "fs.write".to_string(),
            "fs.read".to_string(),
            "db.read".to_string(),
        ];
        let (added, removed) = capability_diff(&prior, &new);
        assert_eq!(added, vec!["db.read".to_string(), "fs.write".to_string()]);
        assert_eq!(removed, vec!["net.get".to_string()]);
    }

    #[test]
    fn capability_diff_ignores_order_and_duplicates() {
        let prior = ["a".to_string(), "b".to_string(), "a".to_string()];
        let new = ["b".to_string(), "a".to_string()];
        let (added, removed) = capability_diff(&prior, &new);
        assert!(added.is_empty());
        assert!(removed.is_empty());
    }
}
