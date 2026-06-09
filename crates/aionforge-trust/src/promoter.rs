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
//! **Author exclusion (scoped).** The spec's "independent attestations" is realized here as
//! distinct *attesters* (one signed vote per agent). Excluding a fact's own author would
//! need an authorship edge the substrate does not yet populate (`WRITTEN_BY` is declared but
//! unwired), so it is a documented follow-up; a self-attester is still only one vote and
//! cannot meet `k >= 2` alone.

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
    /// Per-category overrides; the strictest applicable rule governs a mixed-category candidate.
    pub categories: BTreeMap<String, CategoryRule>,
}

impl Default for PromotionPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            default_k: 3,
            default_threshold: 0.95,
            prior_alpha: 1.0,
            prior_beta: 1.0,
            default_category: "reliability".to_string(),
            categories: BTreeMap::new(),
        }
    }
}

impl PromotionPolicy {
    /// Validate the policy when it is on (06 §4): `k >= 2`, `0.5 < threshold <= 1.0`, finite
    /// positive priors, a non-empty default category, and per-category rules held to the same
    /// bounds.
    ///
    /// # Errors
    /// Returns a message naming the offending field when a bound is violated.
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        check_gate(
            "promotion.default_k",
            "promotion.default_threshold",
            self.default_k,
            self.default_threshold,
        )?;
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
        if self.default_category.trim().is_empty() {
            return Err("promotion.default_category must not be empty".to_string());
        }
        for (category, rule) in &self.categories {
            check_gate(
                &format!("promotion.categories.{category}.k"),
                &format!("promotion.categories.{category}.threshold"),
                rule.k,
                rule.threshold,
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

fn check_gate(k_key: &str, threshold_key: &str, k: u64, threshold: f64) -> Result<(), String> {
    if k < 2 {
        return Err(format!(
            "{k_key} must be at least 2 (a quorum of one is not a quorum)"
        ));
    }
    if !(threshold > 0.5 && threshold <= 1.0) {
        return Err(format!("{threshold_key} must be in the range (0.5, 1.0]"));
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

        // Explicit-only: the attester must name a real fact. An unknown fact is a coarse
        // rejection so a prober cannot distinguish a foreign-but-real fact from a missing one.
        let Some(fact_node) = self.store.fact_node_by_id(&req.fact_id)? else {
            self.audit_rejection(req, AuditKind::InvalidSignature, "unknown_fact")?;
            return Err(PromotionError::InvalidAttestation);
        };

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

        let demoted = DemotedFrom {
            temporal: open_window(now),
        };
        let rejected = self.ledger(
            *candidate_fact_id,
            Some(global_id),
            ledger.posterior,
            ledger.k,
            PromotionStatus::Rejected,
            now,
        );
        let demote_audit = self.governance_audit(
            AuditKind::Demote,
            global_id,
            demote_payload(*candidate_fact_id, global_id, ledger.posterior, ledger.k),
            "demote",
            now,
        );
        let quarantine_audit = self.governance_audit(
            AuditKind::Quarantine,
            global_id,
            demote_payload(*candidate_fact_id, global_id, ledger.posterior, ledger.k),
            "quarantine",
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
        Ok(DemotionOutcome::Demoted { global_id })
    }

    /// The reliability-weighted Beta posterior over a candidate's distinct attesters, summed in
    /// canonical attester-id order so the floating-point result is byte-identical on replay.
    fn posterior(&self, attesters: &[AttesterRecord]) -> Result<f64, PromotionError> {
        let mut sorted: Vec<&AttesterRecord> = attesters.iter().collect();
        sorted.sort_by_key(|a| a.attester_id.to_string());
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

    /// The strictest applicable `(k, threshold)`: the max over the categories the candidate's
    /// attestations carry (so a sensitive-category fact is never promoted under a laxer bar),
    /// starting from the defaults.
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

    /// Whether a fact node is in `current_support_facts` — the canonical "still supported" test.
    fn is_current(&self, node: NodeId) -> Result<bool, StoreError> {
        Ok(self
            .store
            .candidate_state_members(CandidateSet::CurrentSupportFacts)?
            .contains(&node))
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
        let key = format!("attest|{}|{}", req.fact_id, req.attester_id);
        AuditEvent {
            identity: system_identity(content_id("attest", &key), &req.attested_at),
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
            identity: system_identity(content_id(tag, &subject.to_string()), now),
            kind,
            subject_id: subject,
            actor_id: subject,
            payload,
            signature: String::new(),
            occurred_at: now.clone(),
        }
    }
}

fn content_id(tag: &str, key: &str) -> Id {
    Id::from_content_hash(format!("{tag}|{key}").as_bytes())
}

fn system_identity(id: Id, now: &Timestamp) -> Identity {
    Identity {
        id,
        ingested_at: now.clone(),
        namespace: Namespace::System,
        expired_at: None,
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

fn demote_payload(candidate: Id, global: Id, posterior: f64, k: u64) -> serde_json::Value {
    serde_json::json!({
        "candidate_fact_id": candidate.to_string(),
        "promoted_fact_id": global.to_string(),
        "reason": "lost_support",
        "posterior": posterior,
        "k": k,
    })
}

/// The audit kind and reason string for a gate rejection.
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
