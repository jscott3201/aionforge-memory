//! The attestation + quorum-promotion orchestrator (06 §4).
//!
//! [`Promoter`] turns signed attestations into promotions. It records a verified
//! attestation, computes the reliability-weighted Beta posterior over the candidate's
//! distinct attesters, and — when the posterior clears the per-category threshold with
//! enough independent attesters — promotes a team fact to `global` through the store's
//! atomic write-set. The inverse, demotion on lost support, quarantines the global copy and
//! leaves the namespace original untouched.
//!
//! The Promoter reads `Agent.trust_scores` but never writes them: maintaining attester
//! reliability is M4.T05's job. Until then every un-scored agent contributes the
//! uninformative `0.5`, so nothing promotes on a cold start until genuinely reliable
//! attesters accrue. The promotion *policy* (`k`, threshold, priors, per-category rules) is
//! injected; the math lives in [`aionforge_domain::trust::beta_posterior`].
//!
//! **Author exclusion (scoped).** The spec's "independent attestations" is realized as
//! distinct *attesters* (one signed vote per agent) gated at `k >= 2` and weighted by the
//! sybil-bounded posterior — that triple *is* the v1 definition of independence (06 §4). A
//! dedicated author-exclusion edge was considered and dropped (`WRITTEN_BY` removed, 02 §5):
//! a self-attester is still only one vote and cannot meet `k >= 2` alone, and authorship,
//! where needed, is read from `Fact -DERIVED_FROM-> Episode.agent_id`, never a parallel edge.

use std::collections::BTreeMap;
use std::sync::Arc;

use aionforge_domain::blocks::Identity;
use aionforge_domain::edges::{About, AttestedBy, DemotedFrom, PromotedTo};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind, Promotion, PromotionStatus};
use aionforge_domain::nodes::semantic::{Fact, FactStatus};
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::trust::beta_posterior;
use aionforge_store::{AttesterRecord, CandidateSet, NodeId, Store, StoreError};

use crate::attest_gate::{AttestError, AttestRejection, AttestationGate};
use crate::system_audit::{content_id, cycle_id, system_identity};

/// A per-category promotion rule: a stricter quorum and posterior bar for a named category.
#[derive(Debug, Clone, PartialEq)]
pub struct CategoryRule {
    /// The required distinct-attester count for this category.
    pub k: u64,
    /// The posterior bar for this category, in `(0.5, 1.0]`.
    pub threshold: f64,
}

/// The quorum-promotion policy the orchestrator applies (06 §4). Mirrors
/// `aionforge-config`'s `PromotionConfig`; the host maps one to the other.
#[derive(Debug, Clone, PartialEq)]
pub struct PromotionPolicy {
    /// Whether promotion runs at all. Off ⇒ the whole attestation/promotion API is inert.
    pub enabled: bool,
    /// The default distinct-attester quorum.
    pub default_k: u64,
    /// The default posterior bar, in `(0.5, 1.0]`.
    pub default_threshold: f64,
    /// The Beta prior `alpha` over a candidate's correctness.
    pub prior_alpha: f64,
    /// The Beta prior `beta`.
    pub prior_beta: f64,
    /// The category bucket an uncategorized attestation falls into.
    pub default_category: String,
    /// Per-category overrides. For a candidate whose attestations span several categories the
    /// effective gate composes the **maximum `k` and the maximum threshold independently** over
    /// the present categories, so the bar may be stricter than any single configured rule and a
    /// sensitive-category fact is never promoted under a laxer count or threshold.
    pub categories: BTreeMap<String, CategoryRule>,
}

impl Default for PromotionPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            default_k: 3,
            // 0.80 is the highest threshold a quorum of `default_k = 3` can actually clear under
            // the uninformative Beta(1, 1) prior: the bounded posterior maxes out at
            // (alpha + k) / (alpha + beta + k) = 4/5 even with perfectly reliable attesters, so a
            // higher default (an earlier 0.95) would be mutually unsatisfiable with k = 3 and
            // promote nothing. `validate` enforces that reachability. A deployment that wants a
            // stricter global bar raises both the count and the threshold together (per category).
            default_threshold: 0.80,
            prior_alpha: 1.0,
            prior_beta: 1.0,
            default_category: "reliability".to_string(),
            categories: BTreeMap::new(),
        }
    }
}

impl PromotionPolicy {
    /// Validate the policy when it is on (06 §4): finite positive priors, `k >= 2`,
    /// `0.5 < threshold <= 1.0`, the threshold **reachable** at that `k` under the prior, a
    /// non-empty default category, and per-category rules held to the same bounds.
    ///
    /// # Errors
    /// Returns a message naming the offending field when a bound is violated.
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        // Priors first: the reachability check below divides by them, so they must be sane before
        // the gate is evaluated.
        if !self.prior_alpha.is_finite() || self.prior_alpha <= 0.0 {
            return Err(
                "promotion.prior_alpha must be a finite value greater than zero".to_string(),
            );
        }
        if !self.prior_beta.is_finite() || self.prior_beta <= 0.0 {
            return Err(
                "promotion.prior_beta must be a finite value greater than zero".to_string(),
            );
        }
        check_gate(
            "promotion.default_k",
            "promotion.default_threshold",
            self.default_k,
            self.default_threshold,
            self.prior_alpha,
            self.prior_beta,
        )?;
        if self.default_category.trim().is_empty() {
            return Err("promotion.default_category must not be empty".to_string());
        }
        for (category, rule) in &self.categories {
            check_gate(
                &format!("promotion.categories.{category}.k"),
                &format!("promotion.categories.{category}.threshold"),
                rule.k,
                rule.threshold,
                self.prior_alpha,
                self.prior_beta,
            )?;
        }
        Ok(())
    }

    /// The `(k, threshold)` rule for a category, falling back to the defaults.
    fn rule_for(&self, category: &str) -> (u64, f64) {
        self.categories
            .get(category)
            .map(|r| (r.k, r.threshold))
            .unwrap_or((self.default_k, self.default_threshold))
    }
}

fn check_gate(
    k_key: &str,
    threshold_key: &str,
    k: u64,
    threshold: f64,
    prior_alpha: f64,
    prior_beta: f64,
) -> Result<(), String> {
    if k < 2 {
        return Err(format!(
            "{k_key} must be at least 2 (a quorum of one is not a quorum)"
        ));
    }
    if !(threshold > 0.5 && threshold <= 1.0) {
        return Err(format!("{threshold_key} must be in the range (0.5, 1.0]"));
    }
    // Cross-field reachability. The reliability-weighted posterior asymptotes to the attesters'
    // quality mean and, with `k` perfectly reliable attesters, maxes out at the prior-shifted
    // mean (prior_alpha + k) / (prior_alpha + prior_beta + k). If that ceiling is below the
    // threshold the two AND-ed gates are mutually unsatisfiable — `k` attesters can never clear
    // the bar, no matter how reliable — so the policy would silently promote nothing and `k`
    // would be misleading. Reject the pairing rather than ship a dead one.
    let max_posterior = (prior_alpha + k as f64) / (prior_alpha + prior_beta + k as f64);
    if threshold > max_posterior {
        return Err(format!(
            "{threshold_key} is unreachable with {k_key} = {k} under the prior (the posterior \
             tops out at {max_posterior:.3}); lower the threshold or raise the count"
        ));
    }
    Ok(())
}

/// A request to attest a fact: who, what, when, and the Ed25519 signature over the canonical
/// `(fact_id, attester_id, attested_at)` payload.
#[derive(Debug, Clone)]
pub struct AttestRequest {
    /// The fact the attester is vouching for (the attester must already know it).
    pub fact_id: Id,
    /// The attesting agent.
    pub attester_id: Id,
    /// When the attestation was made (also the skew-gated instant).
    pub attested_at: Timestamp,
    /// Base64 Ed25519 signature over the canonical attestation payload.
    pub signature_b64: String,
    /// The trust category the attestation applies to, or `None` for the default bucket.
    pub category: Option<String>,
}

/// The result of recording an attestation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttestReceipt {
    /// Whether the attestation was recorded (false only when promotion is disabled).
    pub recorded: bool,
    /// The promoted global fact id, if this attestation pushed the candidate over the bar.
    pub promoted: Option<Id>,
}

/// The outcome of evaluating a candidate for promotion.
#[derive(Debug, Clone, PartialEq)]
pub enum PromotionOutcome {
    /// Promotion is off.
    Disabled,
    /// The fact is not a promotion candidate (unknown, or not a team fact).
    NotApplicable,
    /// The candidate is already promoted.
    AlreadyPromoted {
        /// The existing global copy, if the ledger recorded one.
        global_id: Option<Id>,
    },
    /// The candidate does not yet clear both gates.
    NotYet {
        /// The current posterior.
        posterior: f64,
        /// The current distinct-attester count.
        k: u64,
        /// The required count.
        needed_k: u64,
        /// The required posterior.
        threshold: f64,
    },
    /// The candidate was promoted.
    Promoted {
        /// The new global copy.
        global_id: Id,
        /// The posterior at promotion.
        posterior: f64,
        /// The distinct-attester count at promotion.
        k: u64,
    },
}

/// The outcome of evaluating a promoted candidate for demotion.
#[derive(Debug, Clone, PartialEq)]
pub enum DemotionOutcome {
    /// Promotion is off.
    Disabled,
    /// Nothing to demote (never promoted, or still supported).
    NoChange,
    /// The global copy was demoted and quarantined.
    Demoted {
        /// The quarantined global copy.
        global_id: Id,
    },
}

/// Why a promoted fact is being demoted. Carries both the audit-payload reason string and the
/// content-address tags for the paired demote/quarantine audits, kept on one type so a
/// reliability-decay demotion never shares a content id with — or is mistaken for — a structural
/// lost-support one in the audit subgraph. The two demotion triggers are state-disjoint (one fires
/// only while the team original is current, the other only once it is not), so at most one ever
/// applies to a given state; the distinct tags keep them legible across a fact's whole lifetime.
#[derive(Clone, Copy)]
enum DemotionReason {
    /// The team original dropped out of the current-support set (superseded or contradicted).
    LostSupport,
    /// The team original is still current, but its attesters' reliability decayed below the bar.
    ReliabilityDecay,
}

impl DemotionReason {
    /// The `reason` written into the demote/quarantine audit payloads.
    fn reason(self) -> &'static str {
        match self {
            Self::LostSupport => "lost_support",
            Self::ReliabilityDecay => "reliability_decay",
        }
    }

    /// The content-address tag for the `Demote` audit.
    fn demote_tag(self) -> &'static str {
        match self {
            Self::LostSupport => "demote",
            Self::ReliabilityDecay => "demote_reliability",
        }
    }

    /// The content-address tag for the `Quarantine` audit.
    fn quarantine_tag(self) -> &'static str {
        match self {
            Self::LostSupport => "quarantine",
            Self::ReliabilityDecay => "quarantine_reliability",
        }
    }
}

/// Why an attestation or a promotion op failed. The caller-facing variants are deliberately
/// coarse — an unknown attester, a bad signature, and an unknown fact all surface as one
/// rejection, so the substrate is neither an enrollment nor a forge oracle (06 §4).
#[derive(Debug, thiserror::Error)]
pub enum PromotionError {
    /// A store read or write failed.
    #[error("the store operation failed")]
    Store(#[from] StoreError),
    /// The attestation failed a security check (the cause is in the audit, not here).
    #[error("the attestation was rejected")]
    InvalidAttestation,
    /// The attestation timestamp is outside the skew window.
    #[error("the attestation timestamp is outside the clock-skew window")]
    ClockSkew {
        /// The deviation, in milliseconds.
        skew_ms: i64,
        /// The configured tolerance, in milliseconds.
        tolerance_ms: u64,
    },
    /// A backend read failed while resolving the attester's key — an availability fault.
    #[error("attester key resolution failed: {0}")]
    Backend(String),
}

/// The attestation + quorum-promotion orchestrator.
#[derive(Debug)]
pub struct Promoter {
    store: Arc<Store>,
    gate: AttestationGate,
    policy: PromotionPolicy,
}

impl Promoter {
    /// Compose an orchestrator over the store, the signed-attestation gate, and the policy.
    #[must_use]
    pub fn new(store: Arc<Store>, gate: AttestationGate, policy: PromotionPolicy) -> Self {
        Self {
            store,
            gate,
            policy,
        }
    }

    /// Record a signed attestation and, on the same call, evaluate the candidate for promotion
    /// (sync-after-attest). Disabled ⇒ a no-op receipt.
    ///
    /// # Errors
    /// [`PromotionError::InvalidAttestation`] / [`PromotionError::ClockSkew`] for a refused
    /// attestation (each audited); [`PromotionError::Backend`] for an availability fault (not
    /// audited); [`PromotionError::Store`] for a store failure.
    pub fn attest(&self, req: &AttestRequest) -> Result<AttestReceipt, PromotionError> {
        if !self.policy.enabled {
            return Ok(AttestReceipt {
                recorded: false,
                promoted: None,
            });
        }

        // Gate first (skew → key resolution → signature), over only the caller-supplied fields.
        // A replayed or unsigned attestation is dropped before any store read, and the
        // fact/attester probes below are reachable only by an already-authenticated attester — so
        // a garbage request can't drive an unauthenticated audit write (skew-first, 06 §4).
        match self.gate.admit(
            &req.fact_id,
            &req.attester_id,
            &req.attested_at,
            &req.signature_b64,
        ) {
            Ok(()) => {}
            Err(AttestError::Rejected(rejection)) => {
                let (kind, reason) = rejection_audit(&rejection);
                self.audit_rejection(req, kind, reason)?;
                return Err(rejection_to_error(rejection));
            }
            Err(AttestError::Backend(error)) => return Err(PromotionError::Backend(error)),
        }

        // Explicit-only: the (now authenticated) attester must name a real fact. An unknown fact
        // is the same coarse rejection so a prober cannot distinguish a foreign-but-real fact from
        // a missing one — but it is now a post-auth rejection, not an unauthenticated one.
        let Some(fact_node) = self.store.fact_node_by_id(&req.fact_id)? else {
            self.audit_rejection(req, AuditKind::InvalidSignature, "unknown_fact")?;
            return Err(PromotionError::InvalidAttestation);
        };

        // The gate resolved the attester's key, so the agent exists; resolve its node for the edge.
        let Some(attester_node) = self.store.agent_node_by_id(&req.attester_id)? else {
            self.audit_rejection(req, AuditKind::InvalidSignature, "unknown_attester")?;
            return Err(PromotionError::InvalidAttestation);
        };

        let edge = AttestedBy {
            attested_at: req.attested_at.clone(),
            signature: req.signature_b64.clone(),
            category: req.category.clone(),
        };
        let audit = self.attest_audit(req);
        self.store
            .attest_fact(fact_node, attester_node, &edge, &audit)?;

        // Sync-after-attest: the attestation instant is the governance `now` for any promotion.
        let promoted = match self.evaluate_promotion(&req.fact_id, &req.attested_at)? {
            PromotionOutcome::Promoted { global_id, .. } => Some(global_id),
            _ => None,
        };
        Ok(AttestReceipt {
            recorded: true,
            promoted,
        })
    }

    /// Evaluate a team fact for promotion, promoting it when the reliability-weighted posterior
    /// clears the strictest applicable threshold with enough distinct attesters. Idempotent.
    ///
    /// # Errors
    /// [`PromotionError::Store`] if a read or the write fails.
    pub fn evaluate_promotion(
        &self,
        fact_id: &Id,
        now: &Timestamp,
    ) -> Result<PromotionOutcome, PromotionError> {
        if !self.policy.enabled {
            return Ok(PromotionOutcome::Disabled);
        }
        let Some(fact_node) = self.store.fact_node_by_id(fact_id)? else {
            return Ok(PromotionOutcome::NotApplicable);
        };
        let Some(team_fact) = self.store.fact_by_node_id(fact_node)? else {
            return Ok(PromotionOutcome::NotApplicable);
        };
        if !matches!(team_fact.identity.namespace, Namespace::Team(_)) {
            return Ok(PromotionOutcome::NotApplicable);
        }
        if let Some(ledger) = self.store.promotion_by_candidate(fact_id)?
            && ledger.status == PromotionStatus::Promoted
        {
            return Ok(PromotionOutcome::AlreadyPromoted {
                global_id: ledger.promoted_fact_id,
            });
        }
        // Current-support precondition — the exact dual of the demotion trigger (02 §9, 06 §4).
        // Only a team fact still in `current_support_facts` may be promoted: a superseded or
        // contradicted fact has lost standing, and its immutable `ATTESTED_BY` votes survive the
        // supersession, so without this guard a retracted (or already-demoted) fact could be
        // promoted — or re-promoted — into `global` on stale attestations.
        if !self.is_current(fact_node)? {
            return Ok(PromotionOutcome::NotApplicable);
        }

        let attesters = self.store.distinct_attesters(fact_node)?;
        let (needed_k, threshold) = self.strictest_rule(&attesters);
        let posterior = self.posterior(&attesters)?;
        let k = attesters.len() as u64;

        if k >= needed_k && posterior >= threshold {
            let global = build_global_copy(&team_fact, now);
            let global_id = global.identity.id;
            let about = About {
                temporal: open_window(now),
            };
            let promoted = PromotedTo {
                temporal: open_window(now),
            };
            let ledger = self.ledger(
                team_fact.identity.id,
                Some(global_id),
                posterior,
                k,
                PromotionStatus::Promoted,
                now,
            );
            let audit = self.governance_audit(
                AuditKind::Promote,
                team_fact.identity.id,
                promote_payload(team_fact.identity.id, posterior, k, threshold),
                "promote",
                now,
            );
            self.store
                .promote_fact(fact_node, &global, &about, &promoted, &ledger, &audit)?;
            Ok(PromotionOutcome::Promoted {
                global_id,
                posterior,
                k,
            })
        } else {
            Ok(PromotionOutcome::NotYet {
                posterior,
                k,
                needed_k,
                threshold,
            })
        }
    }

    /// Evaluate a promoted candidate for demotion on lost support: when the team original has
    /// dropped out of `current_support_facts` (superseded or contradicted), quarantine the
    /// global copy and leave the original untouched. Idempotent.
    ///
    /// # Errors
    /// [`PromotionError::Store`] if a read or the write fails.
    pub fn evaluate_demotion(
        &self,
        candidate_fact_id: &Id,
        now: &Timestamp,
    ) -> Result<DemotionOutcome, PromotionError> {
        if !self.policy.enabled {
            return Ok(DemotionOutcome::Disabled);
        }
        let Some(ledger) = self.store.promotion_by_candidate(candidate_fact_id)? else {
            return Ok(DemotionOutcome::NoChange);
        };
        if ledger.status != PromotionStatus::Promoted {
            return Ok(DemotionOutcome::NoChange);
        }
        let Some(global_id) = ledger.promoted_fact_id else {
            return Ok(DemotionOutcome::NoChange);
        };
        let (Some(global_node), Some(team_node)) = (
            self.store.fact_node_by_id(&global_id)?,
            self.store.fact_node_by_id(candidate_fact_id)?,
        ) else {
            return Ok(DemotionOutcome::NoChange);
        };
        // Still supported ⇒ no demotion. Lost support ⇒ the team original dropped out of the
        // current-support set (a live SUPERSEDED_BY or CONTRADICTS, 02 §9).
        if self.is_current(team_node)? {
            return Ok(DemotionOutcome::NoChange);
        }

        self.demote(
            global_node,
            team_node,
            *candidate_fact_id,
            global_id,
            ledger.posterior,
            ledger.k,
            DemotionReason::LostSupport,
            now,
        )?;
        Ok(DemotionOutcome::Demoted { global_id })
    }

    /// Evaluate a promoted candidate for **reliability-decay demotion**: the team original is still
    /// structurally current, but its attesters' reliability has fallen far enough that the
    /// quorum-weighted posterior no longer clears the strictest applicable threshold. Quarantine
    /// the global copy and leave the team original untouched. Idempotent.
    ///
    /// This is the state-disjoint complement of [`Self::evaluate_demotion`]: that path fires only
    /// once the team original has **lost support** (dropped out of `current_support_facts`); this
    /// one fires only while it is **still current**. The two therefore never both apply to one
    /// state, and they write audits under distinct tags so the subgraph keeps a reliability
    /// demotion apart from a structural one.
    ///
    /// **Refold-first contract.** This reads each attester's *current* `Agent.trust_scores`; it
    /// does not refold them. A caller that wants the verdict to reflect freshly-decayed reliability
    /// (the engine sweep) must refold the attesters before calling, or the recomputed posterior
    /// reads a stale cache and the demotion decision becomes arrival-order dependent.
    ///
    /// # Errors
    /// [`PromotionError::Store`] if a read or the write fails.
    pub fn evaluate_reliability_demotion(
        &self,
        candidate_fact_id: &Id,
        now: &Timestamp,
    ) -> Result<DemotionOutcome, PromotionError> {
        if !self.policy.enabled {
            return Ok(DemotionOutcome::Disabled);
        }
        let Some(ledger) = self.store.promotion_by_candidate(candidate_fact_id)? else {
            return Ok(DemotionOutcome::NoChange);
        };
        if ledger.status != PromotionStatus::Promoted {
            return Ok(DemotionOutcome::NoChange);
        }
        let Some(global_id) = ledger.promoted_fact_id else {
            return Ok(DemotionOutcome::NoChange);
        };
        let (Some(global_node), Some(team_node)) = (
            self.store.fact_node_by_id(&global_id)?,
            self.store.fact_node_by_id(candidate_fact_id)?,
        ) else {
            return Ok(DemotionOutcome::NoChange);
        };
        // Reliability demotion fires only while the team original is STILL current — the exact
        // complement of the lost-support gate in `evaluate_demotion`. A team fact that has dropped
        // out of the support set is the structural path's job; deferring here keeps the two
        // disjoint and never double-demotes one state.
        if !self.is_current(team_node)? {
            return Ok(DemotionOutcome::NoChange);
        }

        // Re-run the promotion gate against the attesters' CURRENT reliability (the caller is
        // responsible for refolding it first). Attestations only accrue, so the live failure mode
        // is the posterior falling below the bar; we still negate the full gate so a tightened
        // policy (a raised `k`) also demotes rather than silently leaving a now-under-quorum fact
        // promoted.
        let attesters = self.store.distinct_attesters(team_node)?;
        let (needed_k, threshold) = self.strictest_rule(&attesters);
        let posterior = self.posterior(&attesters)?;
        let k = attesters.len() as u64;
        if k >= needed_k && posterior >= threshold {
            return Ok(DemotionOutcome::NoChange);
        }

        self.demote(
            global_node,
            team_node,
            *candidate_fact_id,
            global_id,
            posterior,
            k,
            DemotionReason::ReliabilityDecay,
            now,
        )?;
        Ok(DemotionOutcome::Demoted { global_id })
    }

    /// Materialize a demotion through the store's single atomic write-set: close the global copy's
    /// `PROMOTED_TO` into a `DEMOTED_FROM`, quarantine the global node, flip the ledger to
    /// `Rejected`, and write the paired demote + quarantine governance audits. [`DemotionReason`]
    /// supplies the audit-payload reason and the two content-address tags, so a reliability-decay
    /// demotion and a structural one never share an audit id. The store write is idempotent, so a
    /// replayed demotion converges to a no-op.
    #[allow(clippy::too_many_arguments)]
    fn demote(
        &self,
        global_node: NodeId,
        team_node: NodeId,
        candidate_fact_id: Id,
        global_id: Id,
        posterior: f64,
        k: u64,
        reason: DemotionReason,
        now: &Timestamp,
    ) -> Result<(), PromotionError> {
        let demoted = DemotedFrom {
            temporal: open_window(now),
        };
        let rejected = self.ledger(
            candidate_fact_id,
            Some(global_id),
            posterior,
            k,
            PromotionStatus::Rejected,
            now,
        );
        let payload = demote_payload(candidate_fact_id, global_id, posterior, k, reason.reason());
        let demote_audit = self.governance_audit(
            AuditKind::Demote,
            global_id,
            payload.clone(),
            reason.demote_tag(),
            now,
        );
        let quarantine_audit = self.governance_audit(
            AuditKind::Quarantine,
            global_id,
            payload,
            reason.quarantine_tag(),
            now,
        );
        self.store.demote_fact(
            global_node,
            team_node,
            &demoted,
            now,
            &rejected,
            &demote_audit,
            &quarantine_audit,
        )?;
        Ok(())
    }

    /// The reliability-weighted Beta posterior over a candidate's distinct attesters, summed in
    /// canonical attester-id order so the floating-point result is byte-identical on replay.
    fn posterior(&self, attesters: &[AttesterRecord]) -> Result<f64, PromotionError> {
        let mut sorted: Vec<&AttesterRecord> = attesters.iter().collect();
        // Any fixed total order makes the floating-point sum byte-identical on replay; sort by the
        // native (byte-ordered, `Copy`) `Id` so the canonical order costs no per-attester allocation.
        sorted.sort_by_key(|a| a.attester_id);
        let mut reliabilities = Vec::with_capacity(sorted.len());
        for attester in sorted {
            let category = attester
                .category
                .as_deref()
                .unwrap_or(&self.policy.default_category);
            reliabilities.push(self.reliability(&attester.attester_id, category)?);
        }
        let (_, _, score) = beta_posterior(
            self.policy.prior_alpha,
            self.policy.prior_beta,
            &reliabilities,
        );
        Ok(score)
    }

    /// One attester's reliability in a category: its stored Beta score, or the uninformative
    /// `0.5` for an un-scored agent (M4.T05 maintains the scores).
    fn reliability(&self, attester_id: &Id, category: &str) -> Result<f64, PromotionError> {
        Ok(self
            .store
            .agent_by_id(attester_id)?
            .and_then(|agent| agent.trust_scores.0.get(category).map(|c| c.score))
            .unwrap_or(0.5))
    }

    /// The strictest applicable `(k, threshold)`: the **max `k` and the max threshold taken
    /// independently** over the categories the candidate's attestations carry, starting from the
    /// defaults. The two axes can come from different categories, so the effective bar may be
    /// stricter than any single configured rule — a sensitive-category fact is never promoted
    /// under a laxer count or threshold.
    fn strictest_rule(&self, attesters: &[AttesterRecord]) -> (u64, f64) {
        let mut k = self.policy.default_k;
        let mut threshold = self.policy.default_threshold;
        for attester in attesters {
            let category = attester
                .category
                .as_deref()
                .unwrap_or(&self.policy.default_category);
            let (rule_k, rule_threshold) = self.policy.rule_for(category);
            k = k.max(rule_k);
            threshold = threshold.max(rule_threshold);
        }
        (k, threshold)
    }

    /// Whether a fact node is currently supported (02 §9): `status == active` **and** no live
    /// outgoing `SUPERSEDED_BY` / `CONTRADICTS`. The provider expresses only the edge half (it
    /// cannot carry a scalar predicate), so the `active` conjunct is layered here rather than
    /// leaning on the emergent "every status demotion co-writes an excluding edge" coupling — a
    /// future status-only quarantine path would otherwise slip past membership alone.
    fn is_current(&self, node: NodeId) -> Result<bool, StoreError> {
        if !self
            .store
            .candidate_state_members(CandidateSet::CurrentSupportFacts)?
            .contains(&node)
        {
            return Ok(false);
        }
        Ok(self
            .store
            .fact_by_node_id(node)?
            .is_some_and(|fact| fact.status == FactStatus::Active))
    }

    fn ledger(
        &self,
        candidate: Id,
        promoted: Option<Id>,
        posterior: f64,
        k: u64,
        status: PromotionStatus,
        now: &Timestamp,
    ) -> Promotion {
        Promotion {
            identity: system_identity(content_id("promotion", &candidate.to_string()), now),
            candidate_fact_id: candidate,
            posterior,
            k,
            status,
            resolved_at: Some(now.clone()),
            promoted_fact_id: promoted,
        }
    }

    fn attest_audit(&self, req: &AttestRequest) -> AuditEvent {
        // Attestation is not monotonic — a fact can be de-attested and re-attested by the same
        // attester at a later instant — so the audit id folds `attested_at`, keeping each
        // attestation a distinct row in the fact's history while a replay still dedupes.
        let key = format!("attest|{}|{}", req.fact_id, req.attester_id);
        AuditEvent {
            identity: system_identity(cycle_id("attest", &key, &req.attested_at), &req.attested_at),
            kind: AuditKind::Attest,
            subject_id: req.fact_id,
            actor_id: req.attester_id,
            payload: serde_json::json!({
                "fact_id": req.fact_id.to_string(),
                "attester_id": req.attester_id.to_string(),
                "category": req.category,
            }),
            signature: String::new(),
            occurred_at: req.attested_at.clone(),
        }
    }

    fn audit_rejection(
        &self,
        req: &AttestRequest,
        kind: AuditKind,
        reason: &str,
    ) -> Result<(), PromotionError> {
        let key = format!(
            "{reason}|{}|{}|{}",
            req.fact_id,
            req.attester_id,
            req.attested_at.timestamp().as_millisecond()
        );
        let audit = AuditEvent {
            identity: system_identity(content_id("attest_reject", &key), &req.attested_at),
            kind,
            subject_id: req.fact_id,
            actor_id: req.attester_id,
            payload: serde_json::json!({ "reason": reason }),
            signature: String::new(),
            occurred_at: req.attested_at.clone(),
        };
        self.store.commit_audit(&audit)?;
        Ok(())
    }

    fn governance_audit(
        &self,
        kind: AuditKind,
        subject: Id,
        payload: serde_json::Value,
        tag: &str,
        now: &Timestamp,
    ) -> AuditEvent {
        AuditEvent {
            // A subject's governance transitions recur across cycles (promote -> demote ->
            // re-promote), so the audit id folds `now` to keep each transition a distinct row in
            // the subject's history. The promotion ledger and global copy keep their stable
            // content_id — those must resurrect the same node on a re-derivation, not multiply.
            identity: system_identity(cycle_id(tag, &subject.to_string(), now), now),
            kind,
            subject_id: subject,
            actor_id: subject,
            payload,
            signature: String::new(),
            occurred_at: now.clone(),
        }
    }
}

fn open_window(now: &Timestamp) -> BiTemporal {
    BiTemporal {
        valid_from: now.clone(),
        valid_to: None,
        ingested_at: now.clone(),
        expired_at: None,
    }
}

/// The promoted global copy: the team fact cloned into `global` under a lineage-only,
/// content-addressed id, current and active. Its stats are recomputed by the next
/// consolidation pass; only the identity and status change here.
fn build_global_copy(team: &Fact, now: &Timestamp) -> Fact {
    let global_id = content_id("global", &format!("{}|promoted", team.identity.id));
    let mut global = team.clone();
    global.identity = Identity {
        id: global_id,
        ingested_at: now.clone(),
        namespace: Namespace::Global,
        expired_at: None,
    };
    global.status = FactStatus::Active;
    global
}

fn promote_payload(candidate: Id, posterior: f64, k: u64, threshold: f64) -> serde_json::Value {
    serde_json::json!({
        "candidate_fact_id": candidate.to_string(),
        "posterior": posterior,
        "k": k,
        "threshold": threshold,
    })
}

fn demote_payload(
    candidate: Id,
    global: Id,
    posterior: f64,
    k: u64,
    reason: &str,
) -> serde_json::Value {
    serde_json::json!({
        "candidate_fact_id": candidate.to_string(),
        "promoted_fact_id": global.to_string(),
        "reason": reason,
        "posterior": posterior,
        "k": k,
    })
}

/// The audit kind and reason string for a gate rejection. `InvalidSignature` is the umbrella
/// reject-kind here — an unknown attester (and, at the call site, an unknown fact) record under it
/// too, with the true cause in the reason field — matching the deliberately coarse caller error.
/// A dedicated rejection kind, if the audit vocabulary gains one, lands with the M4.T06 subgraph.
fn rejection_audit(rejection: &AttestRejection) -> (AuditKind, &'static str) {
    match rejection {
        AttestRejection::UnknownAttester => (AuditKind::InvalidSignature, "unknown_attester"),
        AttestRejection::BadSignature => (AuditKind::InvalidSignature, "bad_signature"),
        AttestRejection::ClockSkew { .. } => (AuditKind::ClockSkewRejected, "clock_skew"),
    }
}

/// Map a gate rejection to the coarse caller-facing error: unknown-attester and bad-signature
/// collapse to one rejection; clock-skew is reported on its own so an honest client can resync.
fn rejection_to_error(rejection: AttestRejection) -> PromotionError {
    match rejection {
        AttestRejection::UnknownAttester | AttestRejection::BadSignature => {
            PromotionError::InvalidAttestation
        }
        AttestRejection::ClockSkew {
            skew_ms,
            tolerance_ms,
        } => PromotionError::ClockSkew {
            skew_ms,
            tolerance_ms,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::DemotionReason;

    /// The structural and reliability demotions must never share an audit content id, and the
    /// structural tags must stay byte-for-byte what they were before the shared-helper refactor —
    /// the content-addressed audit id is `(tag, subject)`, so a changed tag would either collide
    /// the two paths or silently re-key the existing lost-support audit.
    #[test]
    fn demotion_reason_tags_are_distinct_and_structural_tags_are_pinned() {
        let lost = DemotionReason::LostSupport;
        let decay = DemotionReason::ReliabilityDecay;

        assert_eq!(lost.reason(), "lost_support");
        assert_eq!(lost.demote_tag(), "demote");
        assert_eq!(lost.quarantine_tag(), "quarantine");

        assert_eq!(decay.reason(), "reliability_decay");
        assert_eq!(decay.demote_tag(), "demote_reliability");
        assert_eq!(decay.quarantine_tag(), "quarantine_reliability");

        assert_ne!(lost.reason(), decay.reason());
        assert_ne!(lost.demote_tag(), decay.demote_tag());
        assert_ne!(lost.quarantine_tag(), decay.quarantine_tag());
    }
}
