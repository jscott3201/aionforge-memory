//! The active-forgetting orchestrator (05 §2, M5.T02).
//!
//! The [`Forgetter`] composes the layers the design keeps apart: the pure eligibility
//! predicate from the domain (importance/trust/reference/age axes), the store's
//! candidate page and edge probes (the graph axes it cannot see from pure code), and the
//! soft-forget write primitives. Every decision is conservative — each axis can only
//! spare — and every applied forget carries an audit event addressed to the memory's own
//! namespace recording the decision basis, so the reversible window is explainable.
//!
//! Everything here is **off-cursor** and host-cadence: the engine facade drives the
//! sweep page by page with a caller-supplied `now`, exactly like the reliability decay
//! sweep, and a crash loses at most the in-flight node's evaluation (it re-runs on the
//! next page; the store write gate makes any survivor a no-op).

use std::sync::Arc;

use aionforge_domain::decay::{
    ForgetAxes, ForgetFloors, Tier, decayed_importance, forget_eligible, tier_for_label,
};
use aionforge_domain::edges::{
    AttestedBy, DemotedFrom, DependsOn, DerivedFrom, HasFailure, Mentions, PromotedTo, RelatesTo,
    Supports,
};
use aionforge_domain::ids::Id;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::core::CoreBlock;
use aionforge_domain::nodes::episodic::Episode;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::procedural::{BadPattern, Skill};
use aionforge_domain::nodes::semantic::{Entity, Fact};
use aionforge_domain::time::Timestamp;
use aionforge_store::{ForgetCandidate, ForgetCursor, ForgetWrite, Store, StoreError};

use crate::audit_addr::{cycle_id, namespace_identity, substrate_actor};
use crate::policy::ForgettingPolicy;

/// The incoming-edge allowlist that marks a memory as still depended upon — the
/// "unreferenced" axis probes exactly these (05 §2). `AUDIT`, provenance, scope, and
/// session wiring deliberately do not protect: every memory has those, and a protecting
/// allowlist that matches everything forgets nothing.
const PROTECTING_REFERENCES: [&str; 6] = [
    DerivedFrom::LABEL,
    Supports::LABEL,
    DependsOn::LABEL,
    RelatesTo::LABEL,
    HasFailure::LABEL,
    Mentions::LABEL,
];

/// Promotion-lineage labels, either direction. A node on this lineage belongs to
/// governance: `needs_resurrection` keys on bare `expired_at`, so a re-promotion would
/// silently clear a soft-forget, and a demotion would overwrite one. Exclusion removes
/// both collision modes without touching the promotion path.
const LINEAGE: [&str; 2] = [PromotedTo::LABEL, DemotedFrom::LABEL];

/// The kinds a point-forget may reach: the sweep's two plus `Skill`, whose retrieval
/// already honors `expired_at` (deprecate-never-delete owns its versioning, so the sweep
/// stays out, but a host may point-forget one). `BadPattern` joins only behind the
/// policy toggle; identity memory and the deferred kinds are protected.
const POINT_LABELS: [&str; 3] = [Episode::LABEL, Fact::LABEL, Skill::LABEL];

/// Every `Stats`-bearing kind, for resolution: a point op must find a protected memory
/// to *say* it is protected rather than claiming it does not exist. The pin/unpin
/// surface resolves over the same set — pin works on everything that has a pin field.
pub(crate) const ALL_MEMORY_LABELS: [&str; 7] = [
    Episode::LABEL,
    Fact::LABEL,
    Entity::LABEL,
    Note::LABEL,
    Skill::LABEL,
    BadPattern::LABEL,
    CoreBlock::LABEL,
];

/// Why a memory was spared. Reported, never silent — a host that asked for a forget
/// learns which protection held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpareReason {
    /// The kind is not forgettable on this path (identity memory, deferred kinds, or
    /// `BadPattern` without its toggle).
    ProtectedKind,
    /// Pinned: never forgettable, at any floor, on any path.
    Pinned,
    /// On the promotion lineage; governance owns its lifecycle.
    PromotionLineage,
    /// Carries an attestation; refused entirely until the M5.T03 cascade owns it.
    Attested,
    /// Decayed importance holds at or above the floor.
    ImportanceHolds,
    /// Trust holds at or above the floor.
    TrustHolds,
    /// A live protecting reference still points here.
    Referenced,
    /// Younger than the minimum age.
    TooYoung,
    /// The store refused: another revision channel owns the node's status.
    StatusOwned,
}

/// One eligibility decision over a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgetDecision {
    /// Every axis is low and no exemption fired.
    Forget,
    /// Spared, with the first protection that held.
    Spare(SpareReason),
}

/// The outcome of a point-forget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointForget {
    /// Soft-forgotten and audited.
    Forgotten,
    /// Already expired; nothing changed, nothing audited.
    AlreadyForgotten,
    /// No memory carries this id.
    NotFound,
    /// Protected; the reason names the axis or exemption that held.
    Protected(SpareReason),
    /// Forgetting is not enabled; nothing was read or written. The honest answer to a
    /// host calling a switched-off surface — never a fabricated "not found".
    Disabled,
}

/// The outcome of a point-unforget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointUnforget {
    /// Restored into default retrieval and audited.
    Restored,
    /// Not forgotten in the first place; nothing changed, nothing audited.
    NotForgotten,
    /// No memory carries this id.
    NotFound,
    /// The expiry belongs to another channel (the demotion shape); refused.
    Protected(SpareReason),
    /// Forgetting is not enabled; nothing was read or written.
    Disabled,
}

/// One swept page's tally.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForgetSweepPage {
    /// Candidates evaluated on this page.
    pub scanned: usize,
    /// Candidates soft-forgotten and audited.
    pub forgotten: usize,
    /// Candidates spared by an exemption or a holding axis.
    pub spared: usize,
    /// Where the next page resumes, or `None` when the scan is complete.
    pub next: Option<ForgetCursor>,
}

/// The active-forgetting orchestrator. Held by the engine as an `Option` — absent means
/// off, and every facade method is inert without reading the graph.
pub struct Forgetter {
    store: Arc<Store>,
    policy: ForgettingPolicy,
}

impl Forgetter {
    /// Build over the store with a validated policy.
    #[must_use]
    pub fn new(store: Arc<Store>, policy: ForgettingPolicy) -> Self {
        Self { store, policy }
    }

    /// The policy this forgetter runs.
    #[must_use]
    pub fn policy(&self) -> &ForgettingPolicy {
        &self.policy
    }

    /// Evaluate one candidate against every protection and axis (05 §2). Pure axes run
    /// through the domain predicate; graph axes probe the store. Ordered cheap-first;
    /// the first protection that holds is the reported reason.
    ///
    /// # Errors
    /// Returns [`StoreError`] if an edge probe fails.
    pub fn evaluate(
        &self,
        candidate: &ForgetCandidate,
        now: &Timestamp,
    ) -> Result<ForgetDecision, StoreError> {
        if candidate.identity.expired_at.is_some() {
            // Already out of default recall; the sweep page filters these, so this arm
            // serves point ops and races.
            return Ok(ForgetDecision::Spare(SpareReason::StatusOwned));
        }
        if candidate.stats.is_pinned {
            return Ok(ForgetDecision::Spare(SpareReason::Pinned));
        }
        if self.store.has_adjacent_edge(candidate.node, &LINEAGE)? {
            return Ok(ForgetDecision::Spare(SpareReason::PromotionLineage));
        }
        if self
            .store
            .has_adjacent_edge(candidate.node, &[AttestedBy::LABEL])?
        {
            return Ok(ForgetDecision::Spare(SpareReason::Attested));
        }

        let half_life = match tier_for_label(&candidate.label) {
            Some(Tier::Episodic) => self.policy.episodic_half_life_secs,
            Some(Tier::Semantic) => self.policy.semantic_half_life_secs,
            None => return Ok(ForgetDecision::Spare(SpareReason::ProtectedKind)),
        };
        let decayed = decayed_importance(
            candidate.stats.importance,
            &candidate.stats.last_access,
            now,
            half_life,
            candidate.stats.is_pinned,
        );
        let unreferenced = !self
            .store
            .has_protecting_reference(candidate.node, &PROTECTING_REFERENCES)?;
        let age_secs = (now.timestamp().as_second()
            - candidate.identity.ingested_at.timestamp().as_second())
        .max(0);
        let axes = ForgetAxes {
            is_pinned: candidate.stats.is_pinned,
            decayed,
            trust: candidate.stats.trust,
            unreferenced,
            age_secs,
        };
        let floors = ForgetFloors {
            importance_floor: self.policy.importance_floor,
            trust_floor: self.policy.trust_floor,
            min_age_secs: i64::try_from(self.policy.min_age_secs).unwrap_or(i64::MAX),
        };
        if forget_eligible(&axes, &floors) {
            return Ok(ForgetDecision::Forget);
        }
        // Report the first axis that held, in the predicate's own order. The decision
        // above is the single authority; this is explanation only.
        let reason = if !(decayed.is_finite() && decayed < floors.importance_floor) {
            SpareReason::ImportanceHolds
        } else if !(candidate.stats.trust.is_finite() && candidate.stats.trust < floors.trust_floor)
        {
            SpareReason::TrustHolds
        } else if !unreferenced {
            SpareReason::Referenced
        } else {
            SpareReason::TooYoung
        };
        Ok(ForgetDecision::Spare(reason))
    }

    /// Sweep one candidate page: evaluate every candidate, soft-forget the eligible,
    /// and tally (05 §2). The caller supplies the page cursor and `now`; the page size
    /// is the smaller of `limit` and the policy's batch cap.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a read, probe, or write fails. A failure loses at most
    /// the in-flight node; everything already applied is committed and idempotent.
    pub fn sweep_page(
        &self,
        after: Option<&ForgetCursor>,
        limit: usize,
        now: &Timestamp,
    ) -> Result<ForgetSweepPage, StoreError> {
        let limit = limit.min(self.policy.batch_cap).max(1);
        let page = self.store.forgettable_candidates(after, limit)?;
        let mut report = ForgetSweepPage {
            next: page.next,
            ..ForgetSweepPage::default()
        };
        for candidate in &page.candidates {
            report.scanned += 1;
            match self.evaluate(candidate, now)? {
                ForgetDecision::Forget => {
                    let audit = self.forget_audit(candidate, now, "active_forgetting_sweep");
                    match self.store.soft_forget(candidate.node, now, &audit)? {
                        ForgetWrite::Applied => report.forgotten += 1,
                        ForgetWrite::Noop | ForgetWrite::RefusedStatus => report.spared += 1,
                    }
                }
                ForgetDecision::Spare(_) => report.spared += 1,
            }
        }
        Ok(report)
    }

    /// Point-forget one memory by id, fully gated by the same predicate as the sweep —
    /// a host cannot force-forget a pinned, attested, lineage, or protected-kind memory
    /// (05 §2); it learns which protection held instead.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a read, probe, or write fails.
    pub fn forget(&self, id: &Id, now: &Timestamp) -> Result<PointForget, StoreError> {
        let Some(candidate) = self.store.memory_by_id(id, &ALL_MEMORY_LABELS)? else {
            return Ok(PointForget::NotFound);
        };
        if candidate.identity.expired_at.is_some() {
            return Ok(PointForget::AlreadyForgotten);
        }
        if !self.point_admitted(&candidate.label) {
            return Ok(PointForget::Protected(SpareReason::ProtectedKind));
        }
        match self.evaluate(&candidate, now)? {
            ForgetDecision::Spare(reason) => Ok(PointForget::Protected(reason)),
            ForgetDecision::Forget => {
                let audit = self.forget_audit(&candidate, now, "manual");
                match self.store.soft_forget(candidate.node, now, &audit)? {
                    ForgetWrite::Applied => Ok(PointForget::Forgotten),
                    ForgetWrite::Noop => Ok(PointForget::AlreadyForgotten),
                    ForgetWrite::RefusedStatus => {
                        Ok(PointForget::Protected(SpareReason::StatusOwned))
                    }
                }
            }
        }
    }

    /// Reverse a soft-forget by id (05 §2). No eligibility gate on the way back —
    /// restoring a memory is always safe — but the demotion shape stays refused (that
    /// expiry belongs to governance).
    ///
    /// # Errors
    /// Returns [`StoreError`] if a read or write fails.
    pub fn unforget(&self, id: &Id, now: &Timestamp) -> Result<PointUnforget, StoreError> {
        let Some(candidate) = self.store.memory_by_id(id, &ALL_MEMORY_LABELS)? else {
            return Ok(PointUnforget::NotFound);
        };
        if candidate.identity.expired_at.is_none() {
            return Ok(PointUnforget::NotForgotten);
        }
        let audit = AuditEvent {
            identity: namespace_identity(
                cycle_id("unforget", id, now),
                candidate.identity.namespace.clone(),
                now,
            ),
            kind: AuditKind::Unforget,
            subject_id: *id,
            actor_id: substrate_actor(),
            payload: serde_json::json!({
                "reason": "manual_unforget",
                "kind": candidate.label,
            }),
            signature: String::new(),
            occurred_at: now.clone(),
        };
        match self.store.unforget(candidate.node, &audit)? {
            ForgetWrite::Applied => Ok(PointUnforget::Restored),
            ForgetWrite::Noop => Ok(PointUnforget::NotForgotten),
            ForgetWrite::RefusedStatus => Ok(PointUnforget::Protected(SpareReason::StatusOwned)),
        }
    }

    /// Whether a kind is admitted on the point-forget path.
    fn point_admitted(&self, label: &str) -> bool {
        POINT_LABELS.contains(&label)
            || (label == BadPattern::LABEL && self.policy.forget_bad_patterns)
    }

    /// The forget audit event: cycle-addressed, in the memory's own namespace, recording
    /// the decision basis so the reversible window is explainable.
    fn forget_audit(
        &self,
        candidate: &ForgetCandidate,
        now: &Timestamp,
        reason: &str,
    ) -> AuditEvent {
        let tier = match tier_for_label(&candidate.label) {
            Some(Tier::Episodic) => "episodic",
            _ => "semantic",
        };
        let half_life = if tier == "episodic" {
            self.policy.episodic_half_life_secs
        } else {
            self.policy.semantic_half_life_secs
        };
        let decayed = decayed_importance(
            candidate.stats.importance,
            &candidate.stats.last_access,
            now,
            half_life,
            candidate.stats.is_pinned,
        );
        AuditEvent {
            identity: namespace_identity(
                cycle_id("forget", &candidate.identity.id, now),
                candidate.identity.namespace.clone(),
                now,
            ),
            kind: AuditKind::Forget,
            subject_id: candidate.identity.id,
            actor_id: substrate_actor(),
            payload: serde_json::json!({
                "reason": reason,
                "kind": candidate.label,
                "tier": tier,
                "decayed_importance": decayed,
                "importance_floor": self.policy.importance_floor,
                "trust": candidate.stats.trust,
                "trust_floor": self.policy.trust_floor,
            }),
            signature: String::new(),
            occurred_at: now.clone(),
        }
    }
}
