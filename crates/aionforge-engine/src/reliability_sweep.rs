//! The automatic D1 reliability-decay sweep (06 §5, M4.T05 PR-E2).
//!
//! M4.T05 shipped the host-driven wrappers: the host notices a contradiction quarantine and
//! calls [`Memory::record_reliability_decay`] with the victim's id. This module closes that
//! gap — the engine reads the committed contradiction-quarantine audit rows itself and emits
//! the producer-decay events they imply, off the consolidation cursor, idempotently, on
//! whatever cadence the host calls it (the `evolve_links` off-cursor shape).
//!
//! The audit log doubles as the outbox: every quarantine decision is already a durable,
//! content-addressed row, so the sweep needs no queue, callback, or new persistent state — it
//! re-derives its work from the committed record, and content-addressed decay-event ids make
//! any re-scan (including the full `after = None` rescan) a safe no-op.
//!
//! Only *contradiction* quarantines are D1 triggers. A governance demotion-quarantine (the
//! promoter quarantining a global copy on `lost_support`/`reliability_decay`) is skipped here:
//! its reliability consequence is the D2 attester-decay channel, today host-driven via
//! [`Memory::record_reliability_demotion`]; an auto-D2 sibling would slot in by branching the
//! rows this classifier skips into `demotion_decay` (deferred — coupling a cycle-discriminated
//! trigger to D2's content-keyed effect is its own key-semantics decision).

use aionforge_consolidate::CONTRADICTION_QUARANTINE_REASON;
use aionforge_domain::contracts::Embedder;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::Timestamp;
use aionforge_store::AuditCursor;

use crate::{EngineError, Memory};

/// The outcome of one [`Memory::sweep_reliability_decays`] page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct D1SweepReport {
    /// Contradiction-quarantine rows consumed on this page. Governance demotion-quarantine
    /// rows are skipped and not counted here, though their keyset position still advances
    /// [`D1SweepReport::next`].
    pub quarantines_scanned: usize,
    /// Producer-decay events **newly recorded** on this page — replay no-ops are excluded, so
    /// a re-scan over already-swept rows reads back as zero.
    pub decays_recorded: usize,
    /// Contradiction-quarantine rows whose victim no longer resolves to a fact (hard-purged,
    /// or not a fact at all). Counted and skipped, never an error.
    pub victims_unresolved: usize,
    /// The watermark to persist and pass as `after` on the next call: the position of the last
    /// audit row this page scanned (consumed or skipped), or `None` when the page was empty —
    /// nothing new since `after`. Unlike the page-through audit readers, a non-empty final
    /// page still returns `Some`, so a recurring sweep resumes exactly where it stopped
    /// instead of re-processing a partial tail on every call.
    pub next: Option<AuditCursor>,
}

impl<E: Embedder> Memory<E> {
    /// Sweep committed contradiction-quarantine audit rows and record the D1 producer-decay
    /// events the host would otherwise drive by hand (06 §5) — the engine-discovers-the-ids
    /// dual of [`Memory::record_reliability_decay`]. Off the consolidation cursor.
    ///
    /// Each `AuditKind::Quarantine` row is read from the **all-namespaces L0 spine**, never
    /// the principal-scoped facade: reliability is a global agent property, and a scoped read
    /// would silently drop a victim's team- or private-namespace quarantine and under-penalize
    /// its producers. The sweep therefore takes no `Principal` and runs at the same
    /// substrate-internal authority as the scorer's own reads. Contradiction quarantines decay
    /// the victim's producers; governance demotion-quarantines are skipped (the D2 channel).
    ///
    /// **Idempotent.** Every decay event re-derives the same content-addressed
    /// `(victim, producer)` id the host wrapper mints, so a re-swept row — or one the host
    /// already drove through [`Memory::record_reliability_decay`] — dedups to a no-op under
    /// the store's write lock. There is no double-count window and a full rescan
    /// (`after = None`) is always safe; it is also the heal if a backdated `occurred_at` ever
    /// lands a row behind an already-persisted watermark.
    ///
    /// **Host-driven cadence.** Call it on a timer, at session end, or page-to-empty in a
    /// loop, like [`Memory::evolve_links`]; `limit` is clamped to [`crate::MAX_AUDIT_PAGE`].
    /// Persist [`D1SweepReport::next`] whenever it is `Some` and pass it back as `after`; a
    /// host that always passes `None` is still correct, just pays the rescan. The watermark
    /// resume is exact but not complete under clock regression: rows order by the
    /// host-supplied `occurred_at`, so a row backdated behind the persisted watermark is
    /// invisible to every incremental resume — a watermark-only host must still schedule an
    /// occasional full rescan, which is the heal for that window, not merely a fallback for a
    /// lost cursor. Returns [`D1SweepReport::default`] without reading the log when trust
    /// scoring is off.
    ///
    /// # Errors
    /// [`EngineError::Store`] if the audit page or a victim-fact read fails;
    /// [`EngineError::Reliability`] if building, recording, or refolding the decays fails.
    pub fn sweep_reliability_decays(
        &self,
        after: Option<&AuditCursor>,
        limit: usize,
        now: &Timestamp,
    ) -> Result<D1SweepReport, EngineError> {
        let Some(scorer) = &self.reliability_scorer else {
            return Ok(D1SweepReport::default());
        };
        let page = self
            .store()
            .audit_by_kind(AuditKind::Quarantine, after, limit)?;
        let mut report = D1SweepReport {
            next: page.events.last().map(AuditCursor::of),
            ..D1SweepReport::default()
        };
        for event in &page.events {
            if !is_contradiction_quarantine(event) {
                continue;
            }
            report.quarantines_scanned += 1;
            // Resolve the victim (the audit subject) to a live fact node before handing it to
            // the scorer: a purged or non-fact victim is a counted skip, mirroring the host
            // wrapper's `Ok(0)`, where the scorer itself would error `NotAFact` and abort the
            // rest of the page.
            let Some(node) = self.store().fact_node_by_id(&event.subject_id)? else {
                report.victims_unresolved += 1;
                continue;
            };
            let decays = scorer.quarantine_decay(node, now)?;
            report.decays_recorded += scorer.apply_counting(&decays)?;
        }
        Ok(report)
    }
}

/// Is this stored audit row a *contradiction* quarantine (the D1 trigger), rather than a
/// governance demotion-quarantine (`lost_support`/`reliability_decay`, the D2 channel)?
///
/// The primary contract is the shared [`CONTRADICTION_QUARANTINE_REASON`] symbol — emitter and
/// classifier move together, and the round-trip test pins the match against a genuinely
/// emitted row. The remaining checks are defense in depth against payload drift: a
/// contradiction row names a consolidator pass as its actor (never the victim itself, so
/// `subject_id != actor_id`) and always carries the victim/survivor object pair, while a
/// governance row has `subject_id == actor_id` and a disjoint payload.
fn is_contradiction_quarantine(event: &AuditEvent) -> bool {
    event.kind == AuditKind::Quarantine
        && event
            .payload
            .get("reason")
            .and_then(|reason| reason.as_str())
            == Some(CONTRADICTION_QUARANTINE_REASON)
        && event.subject_id != event.actor_id
        && event.payload.get("victim_object").is_some()
        && event.payload.get("survivor_object").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionforge_domain::blocks::Identity;
    use aionforge_domain::ids::Id;
    use aionforge_domain::namespace::Namespace;

    fn at() -> Timestamp {
        "2026-06-09T09:00:00-05:00[America/Chicago]"
            .parse()
            .expect("valid zoned datetime")
    }

    fn quarantine_row(
        kind: AuditKind,
        subject: Id,
        actor: Id,
        payload: serde_json::Value,
    ) -> AuditEvent {
        AuditEvent {
            identity: Identity {
                id: Id::from_content_hash(b"row"),
                ingested_at: at(),
                namespace: Namespace::Team("acme".to_string()),
                expired_at: None,
            },
            kind,
            subject_id: subject,
            actor_id: actor,
            payload,
            signature: String::new(),
            occurred_at: at(),
        }
    }

    /// The shape the consolidation emitter writes (pinned end-to-end, against the real
    /// emitter, by `a_real_contradiction_quarantine_is_swept_end_to_end` in
    /// `tests/reliability_sweep.rs`; this unit table covers the discriminating axes).
    fn contradiction_payload() -> serde_json::Value {
        serde_json::json!({
            "predicate": "status",
            "victim_object": "down",
            "victim_trust": 0.5,
            "survivor_object": "up",
            "survivor_trust": 0.9,
            "reason": CONTRADICTION_QUARANTINE_REASON,
        })
    }

    #[test]
    fn the_classifier_accepts_a_contradiction_shaped_row() {
        let row = quarantine_row(
            AuditKind::Quarantine,
            Id::from_content_hash(b"victim"),
            Id::from_content_hash(b"pass-actor"),
            contradiction_payload(),
        );
        assert!(is_contradiction_quarantine(&row));
    }

    #[test]
    fn the_classifier_rejects_a_governance_demotion_quarantine() {
        // The promoter's shape: subject == actor == the global copy, demote payload.
        let global = Id::from_content_hash(b"global-copy");
        let row = quarantine_row(
            AuditKind::Quarantine,
            global,
            global,
            serde_json::json!({
                "candidate_fact_id": "x",
                "promoted_fact_id": "y",
                "reason": "lost_support",
                "posterior": 0.4,
                "k": 2,
            }),
        );
        assert!(!is_contradiction_quarantine(&row));
    }

    #[test]
    fn the_classifier_rejects_near_misses_on_every_axis() {
        let victim = Id::from_content_hash(b"victim");
        let actor = Id::from_content_hash(b"pass-actor");
        // Wrong kind.
        let mut row = quarantine_row(AuditKind::Demote, victim, actor, contradiction_payload());
        assert!(!is_contradiction_quarantine(&row));
        row.kind = AuditKind::Quarantine;
        // A reason that drifted from the shared const.
        row.payload["reason"] = serde_json::json!("quarantined for review");
        assert!(!is_contradiction_quarantine(&row));
        row.payload = contradiction_payload();
        // Subject == actor (the governance signature) even with a contradiction payload.
        row.actor_id = victim;
        assert!(!is_contradiction_quarantine(&row));
        row.actor_id = actor;
        // The presence check is not a scalar validator: the emitter's real wire shape for an
        // object is the adjacently-tagged ObjectValue form, and it classifies the same.
        row.payload["victim_object"] = serde_json::json!({"kind": "string", "value": "down"});
        assert!(
            is_contradiction_quarantine(&row),
            "the emitter's tagged object shape classifies"
        );
        let mut gone = row.payload.as_object().expect("object").clone();
        gone.remove("survivor_object");
        row.payload = serde_json::Value::Object(gone);
        assert!(!is_contradiction_quarantine(&row));
    }
}
