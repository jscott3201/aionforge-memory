//! The reliability scorer: turns trust triggers into reliability events and refreshes the caches
//! (06 §5, M4.T05).
//!
//! [`ReliabilityScorer`] is the L2 orchestration over the pure [`ReliabilityFold`] and the L0
//! `trust_fold` surface. It does two jobs:
//!
//! 1. **Build events.** Three triggers each produce content-addressed `ReliabilityUpdate` audit
//!    events as pure data: a *contradiction* quarantine decays the producers of the victim fact, a
//!    *demotion* decays the attesters of the demoted fact, and a distinct-authored *agreement*
//!    rewards the producers of the corroborated fact. The events are returned, not written, so the
//!    cursor path can co-commit them atomically (a consolidation pass is read-only) and a
//!    host-driven path can record them directly.
//! 2. **Refold the caches.** [`ReliabilityScorer::refold_agent`] replays an agent's recorded
//!    events into its `Agent.trust_scores`, then re-derives `Fact.stats.trust` for every fact that
//!    agent produced.
//!
//! Trust is doubly-derived state, so the scorer never invents a stored truth: an agent's score is
//! a fold of its event log, and a fact's trust is a pure function of its producers' folds and its
//! own immutable write-time baseline — never of the value being rewritten.
//!
//! **The fact-trust model** (a ratified refinement of the spec's "MIN over producers" one-liner,
//! 06 §5):
//!
//! ```text
//! trust(F) = min( baseline(F),  MIN over producers a that are GENUINELY DECAYED of score(a, C) )
//! ```
//!
//! where `baseline(F)` is the mean of F's source episodes' immutable write-time trust, `C` is the
//! fact's category, and a producer "genuinely decayed" means its folded reliability sits *below the
//! Beta prior mean*. The two clamps are what make it behave: the outer `baseline` cap means
//! reliability can only ever *sink* a fact, never raise it above its write-time trust; and the
//! prior-pivot means a no-history or freshly-corroborated producer is inert (it contributes the
//! baseline, never binds the MIN), while only a contradicted/invalidated producer pulls the fact
//! down to its own reliability. Both follow the spec intent — "low-trust memories sink" — without
//! the perverse deflation a literal MIN-of-raw-reliabilities would cause.

use std::sync::Arc;

use aionforge_domain::ids::Id;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::semantic::Fact;
use aionforge_domain::time::Timestamp;
use aionforge_store::{NodeId, Store, StoreError};

use crate::reliability::{
    ReliabilityEvent, ReliabilityFold, ReliabilityOutcome, ReliabilityPolicy,
};
use crate::system_audit::{content_id, system_identity};

/// The `(tag, key)` tags for the three triggers' content-addressed event ids. The key disambiguates
/// each distinct decision so a replay dedupes while two genuine decisions stay separate.
const TAG_CONTRADICT: &str = "reliability_contradict";
const TAG_ATTEST_INVALID: &str = "reliability_attest_invalid";
const TAG_AGREE: &str = "reliability_agree";

/// A failure (decay) outcome in the audit payload.
const OUTCOME_FAILURE: &str = "failure";
/// A success (agreement) outcome in the audit payload.
const OUTCOME_SUCCESS: &str = "success";

/// An error from the reliability scorer.
#[derive(Debug, thiserror::Error)]
pub enum ReliabilityError {
    /// A store read or write failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A trigger named a node that is not a fact.
    #[error("the trigger node is not a fact")]
    NotAFact,
}

/// Turns trust triggers into reliability events and refreshes the recomputable caches (06 §5).
///
/// Mirrors [`crate::promoter::Promoter`]'s shape — an `Arc<Store>` plus an injected policy — but
/// needs no signing gate: reliability events are substrate-authored, not signed by an agent.
pub struct ReliabilityScorer {
    store: Arc<Store>,
    policy: ReliabilityPolicy,
}

impl ReliabilityScorer {
    /// Build a scorer over a store and a (validated) reliability policy.
    #[must_use]
    pub fn new(store: Arc<Store>, policy: ReliabilityPolicy) -> Self {
        Self { store, policy }
    }

    // --- event builders (pure data; the caller commits them) --------------------------------

    /// Decay events for the producers of a contradicted-and-quarantined fact (trigger D1).
    ///
    /// Each distinct producing agent of the victim fact takes a `w_contradict` decay in the
    /// victim's category. The event id is content-addressed on `(victim_fact, producer)`, so the
    /// quarantine of one fact decays each producer exactly once on replay.
    ///
    /// # Errors
    /// [`ReliabilityError::NotAFact`] if `victim_fact` is not a fact; [`ReliabilityError::Store`]
    /// on a read failure.
    pub fn quarantine_decay(
        &self,
        victim_fact: NodeId,
        now: &Timestamp,
    ) -> Result<Vec<AuditEvent>, ReliabilityError> {
        let victim = self.fact_of(victim_fact)?;
        let category = self.category_of(&victim);
        let producers = self.store.producing_agents(victim_fact)?;
        Ok(producers
            .iter()
            .map(|producer| {
                let key = format!("{}|{producer}", victim.identity.id);
                self.event(
                    producer,
                    &category,
                    ReliabilityOutcome::Failure,
                    self.policy.w_contradict,
                    TAG_CONTRADICT,
                    &key,
                    &victim.identity.id,
                    "contradiction",
                    now,
                )
            })
            .collect())
    }

    /// Decay events for the attesters of a demoted fact (trigger D2).
    ///
    /// Each distinct attester takes a `w_attest_invalid` decay in the demoted fact's category — the
    /// literal 06 §5 clause "an agent that attests memories later invalidated sees its reliability
    /// decay." The event id is content-addressed on `(demoted_fact, attester)`.
    ///
    /// # Errors
    /// [`ReliabilityError::NotAFact`] if `demoted_fact` is not a fact; [`ReliabilityError::Store`]
    /// on a read failure.
    pub fn demotion_decay(
        &self,
        demoted_fact: NodeId,
        now: &Timestamp,
    ) -> Result<Vec<AuditEvent>, ReliabilityError> {
        let demoted = self.fact_of(demoted_fact)?;
        let category = self.category_of(&demoted);
        let attesters = self.store.distinct_attesters(demoted_fact)?;
        Ok(attesters
            .iter()
            .map(|attester| {
                let key = format!("{}|{}", demoted.identity.id, attester.attester_id);
                self.event(
                    &attester.attester_id,
                    &category,
                    ReliabilityOutcome::Failure,
                    self.policy.w_attest_invalid,
                    TAG_ATTEST_INVALID,
                    &key,
                    &demoted.identity.id,
                    "demotion",
                    now,
                )
            })
            .collect())
    }

    /// Agreement-gain events for the producers of a corroborated fact (trigger G1).
    ///
    /// When a later canonical `corroborating_fact` carries what `asserted_fact`'s producers earlier
    /// claimed, each of those producers earns a `w_agree` gain in the asserted fact's category —
    /// **except** a producer who also authored the corroborating fact. That distinct-author guard
    /// is the anti-farming rule: an agent cannot raise its own reliability by re-asserting itself.
    /// The event id is content-addressed on `(producer, corroborating_fact)`, so distinct
    /// corroborations of the same producer each count once.
    ///
    /// # Errors
    /// [`ReliabilityError::NotAFact`] if either node is not a fact; [`ReliabilityError::Store`] on a
    /// read failure.
    pub fn agreement_gain(
        &self,
        asserted_fact: NodeId,
        corroborating_fact: NodeId,
        now: &Timestamp,
    ) -> Result<Vec<AuditEvent>, ReliabilityError> {
        let asserted = self.fact_of(asserted_fact)?;
        let corroborating = self.fact_of(corroborating_fact)?;
        let category = self.category_of(&asserted);
        let asserted_producers = self.store.producing_agents(asserted_fact)?;
        let corroborating_producers = self.store.producing_agents(corroborating_fact)?;
        Ok(asserted_producers
            .iter()
            .filter(|producer| !corroborating_producers.contains(producer))
            .map(|producer| {
                let key = format!("{producer}|{}", corroborating.identity.id);
                self.event(
                    producer,
                    &category,
                    ReliabilityOutcome::Success,
                    self.policy.w_agree,
                    TAG_AGREE,
                    &key,
                    &corroborating.identity.id,
                    "agreement",
                    now,
                )
            })
            .collect())
    }

    // --- recording + refold (off-cursor writes) ---------------------------------------------

    /// Record reliability events idempotently, then refold every agent they touched (the
    /// host-driven path).
    ///
    /// Each event is written through the L0 dedup (a replay is a no-op), then each distinct subject
    /// agent is refolded so its `Agent.trust_scores` and produced facts' `Fact.stats.trust` reflect
    /// the new log. Used by a host-driven trigger (e.g. demotion); the cursor path instead
    /// co-commits the events with the episode flip and refolds from the facade.
    ///
    /// # Errors
    /// [`ReliabilityError::Store`] on a write failure.
    pub fn apply(&self, events: &[AuditEvent]) -> Result<(), ReliabilityError> {
        self.apply_counting(events).map(|_| ())
    }

    /// [`ReliabilityScorer::apply`], reporting how many events were **newly recorded** (replay
    /// no-ops excluded), so an auto-sweep can return a true new-event count and an idempotency
    /// test can assert `0` on a re-scan.
    ///
    /// Recording and refolding are identical to `apply` — in particular the refold still runs
    /// over every touched subject even when nothing was created, so a crash between a prior
    /// record and its refold heals on the re-scan instead of leaving a stale cache behind.
    ///
    /// # Errors
    /// [`ReliabilityError::Store`] on a write failure.
    pub fn apply_counting(&self, events: &[AuditEvent]) -> Result<usize, ReliabilityError> {
        let mut created = 0usize;
        for event in events {
            if self.store.record_reliability_update_created(event)? {
                created += 1;
            }
        }
        let mut agents: Vec<Id> = events.iter().map(|event| event.subject_id).collect();
        agents.sort_unstable();
        agents.dedup();
        for agent in agents {
            self.refold_agent(&agent)?;
        }
        Ok(created)
    }

    /// Refold one agent: replay its reliability events into its `trust_scores`, then re-derive the
    /// `stats.trust` of every fact it produced.
    ///
    /// The agent-score refresh is skipped when the agent has no enrolled node (an unenrolled
    /// producer still has its facts re-derived — fact trust folds the producer's log directly, not
    /// the agent cache). Both writes are write-when-changed, so a refold that moves nothing writes
    /// nothing.
    ///
    /// # Errors
    /// [`ReliabilityError::Store`] on a read or write failure.
    pub fn refold_agent(&self, agent_id: &Id) -> Result<(), ReliabilityError> {
        // Refresh the agent's cached score from its event log, when the agent is enrolled.
        if let Some(mut agent) = self.store.agent_by_id(agent_id)? {
            agent.trust_scores = self.fold_agent(agent_id)?;
            self.store.refresh_agent_trust(&agent)?;
        }
        // Re-derive each produced fact's trust from primary state.
        for fact_node in self.store.facts_produced_by(agent_id)? {
            if let Some(trust) = self.recompute_fact_trust(fact_node)? {
                self.store.refresh_fact_trust(fact_node, trust)?;
            }
        }
        Ok(())
    }

    // --- internals --------------------------------------------------------------------------

    /// Re-derive a fact's `stats.trust` from primary state: the baseline-anchored conservative MIN.
    ///
    /// Returns `None` (and the caller leaves the fact untouched) when the fact has no episode source
    /// to anchor a baseline to. Reads the source episodes' write-time trust, the producers' event
    /// logs, and the producing-agent edges; the fact node itself is read only for its predicate,
    /// never for its own `stats.trust` (no ratchet on the value being recomputed).
    fn recompute_fact_trust(&self, fact_node: NodeId) -> Result<Option<f64>, ReliabilityError> {
        let Some(baseline) = self.store.fact_source_trust_mean(fact_node)? else {
            return Ok(None);
        };
        let category = self.category_of(&self.fact_of(fact_node)?);
        let prior_mean =
            self.policy.prior_alpha / (self.policy.prior_alpha + self.policy.prior_beta);
        let mut scores = Vec::new();
        for producer in self.store.producing_agents(fact_node)? {
            // Re-fold each producer from its PRIMARY event log (not the cached blob), so the
            // recompute stays a pure function of the committed graph regardless of cache freshness.
            scores.push(
                self.fold_agent(&producer)?
                    .0
                    .get(&category)
                    .map(|c| c.score),
            );
        }
        Ok(Some(fact_trust(baseline, prior_mean, scores.into_iter())))
    }

    /// Fold an agent's recorded reliability events into per-category trust scores.
    fn fold_agent(
        &self,
        agent_id: &Id,
    ) -> Result<aionforge_domain::nodes::agent::TrustScores, ReliabilityError> {
        let audits = self.store.reliability_events(agent_id)?;
        let events: Vec<ReliabilityEvent> = audits.iter().filter_map(decode_event).collect();
        Ok(ReliabilityFold::fold(&self.policy, &events))
    }

    /// The fact's trust category: its predicate bucket, falling back to the policy default when the
    /// predicate is empty. One shared resolver, used both to *emit* events and to *recompute* fact
    /// trust, so the producer's scored category and the fact's looked-up category always align.
    fn category_of(&self, fact: &Fact) -> String {
        if fact.predicate.trim().is_empty() {
            self.policy.default_category.clone()
        } else {
            fact.predicate.clone()
        }
    }

    /// Read a fact node, erroring if it is not a fact.
    fn fact_of(&self, node: NodeId) -> Result<Fact, ReliabilityError> {
        self.store
            .fact_by_node_id(node)?
            .ok_or(ReliabilityError::NotAFact)
    }

    /// Build one `ReliabilityUpdate` audit event: subject is the agent whose score moves; the
    /// triggering fact, category, outcome, and weight ride in the payload for the fold to decode.
    #[allow(clippy::too_many_arguments)]
    fn event(
        &self,
        agent: &Id,
        category: &str,
        outcome: ReliabilityOutcome,
        weight: f64,
        tag: &str,
        key: &str,
        trigger_fact: &Id,
        trigger: &str,
        now: &Timestamp,
    ) -> AuditEvent {
        let outcome_str = match outcome {
            ReliabilityOutcome::Success => OUTCOME_SUCCESS,
            ReliabilityOutcome::Failure => OUTCOME_FAILURE,
        };
        AuditEvent {
            identity: system_identity(content_id(tag, key), now),
            kind: AuditKind::ReliabilityUpdate,
            subject_id: *agent,
            // Substrate-computed, so the actor is the subject agent (mirrors the promoter's
            // governance audits); there is no separate signing principal for a reliability update.
            actor_id: *agent,
            payload: serde_json::json!({
                "category": category,
                "outcome": outcome_str,
                "weight": weight,
                "trigger_fact": trigger_fact.to_string(),
                "trigger": trigger,
            }),
            signature: String::new(),
            occurred_at: now.clone(),
        }
    }
}

/// The baseline-anchored conservative MIN: a fact's `stats.trust` from its write-time `baseline`,
/// the Beta `prior_mean`, and each distinct producer's folded category reliability (`None` = the
/// producer has no events in this category).
///
/// A producer binds the MIN only when **genuinely decayed** — its score is below the prior mean. A
/// no-history producer (`None`) or a gained/neutral one (score `>=` prior mean) is inert and leaves
/// the baseline standing. So reliability can only ever *sink* a fact below its write-time trust,
/// never raise it, and a neutral co-producer never pins a healthy fact down. Pure and
/// order-independent (`f64::min` commutes), so the result is a function of the producer *set*.
fn fact_trust(
    baseline: f64,
    prior_mean: f64,
    producer_scores: impl Iterator<Item = Option<f64>>,
) -> f64 {
    let mut trust = baseline;
    for score in producer_scores {
        if let Some(score) = score
            && score < prior_mean
        {
            trust = trust.min(score);
        }
    }
    trust
}

/// Decode a recorded `ReliabilityUpdate` audit event into the fold's input. Returns `None` for any
/// other kind or a malformed payload, so a foreign or corrupt event is skipped, not folded.
fn decode_event(audit: &AuditEvent) -> Option<ReliabilityEvent> {
    if audit.kind != AuditKind::ReliabilityUpdate {
        return None;
    }
    let category = audit.payload.get("category")?.as_str()?.to_owned();
    let outcome = match audit.payload.get("outcome")?.as_str()? {
        OUTCOME_SUCCESS => ReliabilityOutcome::Success,
        OUTCOME_FAILURE => ReliabilityOutcome::Failure,
        _ => return None,
    };
    let weight = audit.payload.get("weight")?.as_f64()?;
    Some(ReliabilityEvent {
        event_id: audit.identity.id,
        category,
        outcome,
        weight,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionforge_domain::blocks::Identity;
    use aionforge_domain::namespace::Namespace;

    const EPS: f64 = 1e-12;
    /// The uniform Beta(1, 1) prior mean.
    const PRIOR: f64 = 0.5;

    fn scores(values: &[Option<f64>]) -> impl Iterator<Item = Option<f64>> + '_ {
        values.iter().copied()
    }

    // --- the baseline-anchored conservative MIN (the ratified recompute model) ---------------

    #[test]
    fn a_no_history_producer_holds_the_baseline() {
        assert!((fact_trust(0.8, PRIOR, scores(&[None])) - 0.8).abs() < EPS);
    }

    #[test]
    fn a_gained_producer_holds_the_baseline_and_never_deflates() {
        // One agreement ⇒ ~0.5556, at or above the prior ⇒ inert; the healthy fact stays at 0.8.
        assert!((fact_trust(0.8, PRIOR, scores(&[Some(1.25 / 2.25)])) - 0.8).abs() < EPS);
        // A heavily-gained producer (0.9) still cannot raise the fact above its write-time trust.
        assert!((fact_trust(0.8, PRIOR, scores(&[Some(0.9)])) - 0.8).abs() < EPS);
    }

    #[test]
    fn one_contradiction_sinks_to_the_producer_reliability() {
        // One contradiction ⇒ 1/3, below the prior ⇒ binds; the fact sinks to it.
        assert!((fact_trust(0.8, PRIOR, scores(&[Some(1.0 / 3.0)])) - 1.0 / 3.0).abs() < EPS);
        // Two contradictions ⇒ 0.25, sinks further.
        assert!((fact_trust(0.8, PRIOR, scores(&[Some(0.25)])) - 0.25).abs() < EPS);
    }

    #[test]
    fn a_neutral_co_producer_does_not_pin_a_decayed_fact_to_the_prior() {
        // A decayed producer (0.333) and a no-history co-producer (None): the fact sinks to 0.333,
        // NOT to the 0.5 prior — the naive-MIN failure this model fixes.
        let t = fact_trust(0.8, PRIOR, scores(&[Some(1.0 / 3.0), None]));
        assert!((t - 1.0 / 3.0).abs() < EPS);
    }

    #[test]
    fn the_min_takes_the_worst_of_several_decayed_producers() {
        let t = fact_trust(0.8, PRIOR, scores(&[Some(1.0 / 3.0), Some(0.25)]));
        assert!((t - 0.25).abs() < EPS);
    }

    #[test]
    fn no_producers_leaves_the_baseline() {
        assert!((fact_trust(0.8, PRIOR, scores(&[])) - 0.8).abs() < EPS);
    }

    #[test]
    fn a_low_baseline_neutral_producer_is_inert() {
        // Baseline 0.3 (below the prior); a no-history producer must not drag it toward 0.5.
        assert!((fact_trust(0.3, PRIOR, scores(&[None])) - 0.3).abs() < EPS);
    }

    #[test]
    fn the_aggregation_is_order_independent() {
        let forward = fact_trust(0.8, PRIOR, scores(&[Some(1.0 / 3.0), Some(0.25), None]));
        let reverse = fact_trust(0.8, PRIOR, scores(&[None, Some(0.25), Some(1.0 / 3.0)]));
        assert!((forward - reverse).abs() < EPS);
    }

    #[test]
    fn the_prior_pivot_uses_the_configured_prior_not_a_hardcoded_half() {
        // Beta(2, 5) ⇒ prior mean 2/7 ≈ 0.2857. A producer at 0.30 is ABOVE it ⇒ inert; one at
        // 0.25 is below ⇒ binds. A hardcoded 0.5 pivot would wrongly bind the 0.30 producer.
        let prior = 2.0 / 7.0;
        assert!((fact_trust(0.5, prior, scores(&[Some(0.30)])) - 0.5).abs() < EPS);
        assert!((fact_trust(0.5, prior, scores(&[Some(0.25)])) - 0.25).abs() < EPS);
    }

    // --- decode_event -----------------------------------------------------------------------

    fn audit(kind: AuditKind, payload: serde_json::Value) -> AuditEvent {
        let id = content_id("test", "event");
        AuditEvent {
            identity: Identity {
                id,
                ingested_at: "2026-06-08T09:00:00-05:00[America/Chicago]"
                    .parse()
                    .expect("ts"),
                namespace: Namespace::System,
                expired_at: None,
            },
            kind,
            subject_id: content_id("test", "agent"),
            actor_id: content_id("test", "agent"),
            payload,
            signature: String::new(),
            occurred_at: "2026-06-08T09:00:00-05:00[America/Chicago]"
                .parse()
                .expect("ts"),
        }
    }

    #[test]
    fn decode_reads_back_a_reliability_update() {
        let event = audit(
            AuditKind::ReliabilityUpdate,
            serde_json::json!({"category": "preferred_by", "outcome": "failure", "weight": 1.0}),
        );
        let decoded = decode_event(&event).expect("decodes");
        assert_eq!(decoded.event_id, event.identity.id);
        assert_eq!(decoded.category, "preferred_by");
        assert_eq!(decoded.outcome, ReliabilityOutcome::Failure);
        assert!((decoded.weight - 1.0).abs() < EPS);
    }

    #[test]
    fn decode_skips_a_foreign_kind_or_malformed_payload() {
        // A non-reliability audit on the same subject is not a reliability event.
        let foreign = audit(
            AuditKind::Capture,
            serde_json::json!({"category": "x", "outcome": "failure", "weight": 1.0}),
        );
        assert!(decode_event(&foreign).is_none());
        // A missing field is skipped rather than panicking.
        let malformed = audit(
            AuditKind::ReliabilityUpdate,
            serde_json::json!({"category": "x", "weight": 1.0}),
        );
        assert!(decode_event(&malformed).is_none());
        // An unknown outcome string is rejected.
        let bad_outcome = audit(
            AuditKind::ReliabilityUpdate,
            serde_json::json!({"category": "x", "outcome": "sideways", "weight": 1.0}),
        );
        assert!(decode_event(&bad_outcome).is_none());
    }
}
