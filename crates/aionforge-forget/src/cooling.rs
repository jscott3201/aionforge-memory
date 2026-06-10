//! The off-cursor cooling sweep (05 §1, M5.T05): stamp core-proximate facts once.
//!
//! Facts materialize in-cursor with `cooled_until = None` — the proximity decision
//! reads attested baselines that are written off-cursor, so an in-cursor stamp would
//! make consolidation replay diverge (00 §52). This sweep runs on the host's cadence
//! instead: it walks recently-ingested facts by `(ingested_at, id)` watermark,
//! compares each embedded fact against the **attested anchors** of the high-trust
//! live core blocks *in its own namespace*, and stamps every proximate fact's
//! `cooled_until` exactly once, with a `Cooled` audit row co-committed on the real
//! transition only.
//!
//! Cooling is the conservative over-approximation 05 §1 asks for: a core block has no
//! subject-predicate-object key, so agree-vs-contradict is not deterministically
//! decidable — every proximate fact cools. Over-cooling costs one fact a few rank
//! positions for one bounded window and self-heals; under-cooling is the actual
//! threat. The anchor is the baseline's `block_embedding` (the identity the quorum
//! co-signed), never the block's mutable current vector: a block without an attested
//! baseline has no anchor and cools nothing. A fact and an anchor in different
//! embedding spaces are never compared — non-proximate by definition.

use aionforge_domain::drift::is_core_proximate;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::{Timestamp, instant_after, to_utc};
use aionforge_store::{CoolWrite, CoolingCursor, StoreError};

use crate::audit_addr::{namespace_identity, transition_id};
use crate::baseline::DriftBaseline;
use crate::detector::{DriftDetector, drift_actor};

/// One [`DriftDetector::sweep_cooling`] page's tally.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoolingSweepReport {
    /// Facts visited on this page (the watermark advances past every one of them,
    /// including the unembedded and the already-stamped).
    pub facts_scanned: usize,
    /// Facts newly stamped and audited on this page.
    pub facts_cooled: usize,
    /// The watermark to persist and pass as `after` on the next call: the position
    /// of the last fact this page visited, or `None` when the page was empty.
    pub next: Option<CoolingCursor>,
}

/// One namespace-scoped anchor: the attested identity a fact is measured against.
struct Anchor {
    block_id: Id,
    embedding: Embedding,
    model: EmbedderModel,
}

impl DriftDetector {
    /// Sweep one page of recently-ingested facts and stamp the core-proximate ones
    /// (05 §1): `cooled_until = now + cooling_window`, once, audited. Idempotent —
    /// an already-stamped fact is visited but never re-stamped or extended, so a
    /// re-scan (including the full `after = None` rescan) is a safe no-op.
    ///
    /// **Host-driven cadence**, the sweep-drift sibling: keep the cooling window at
    /// or above the sweep cadence so a stamped window outlives at least one detector
    /// look. The watermark is exact but not complete under a backdated
    /// `ingested_at`; an occasional fresh pass is the heal, as with the D1 sweep.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the fact page, the block read, or a stamp fails.
    pub fn sweep_cooling(
        &self,
        after: Option<&CoolingCursor>,
        limit: usize,
        now: &Timestamp,
    ) -> Result<CoolingSweepReport, StoreError> {
        if limit == 0 {
            return Ok(CoolingSweepReport::default());
        }
        let candidates = self.store().cooling_candidates(after, limit)?;
        let mut report = CoolingSweepReport {
            next: candidates.last().map(|fact| CoolingCursor {
                ingested_at: fact.ingested_at.clone(),
                id: fact.id,
            }),
            ..CoolingSweepReport::default()
        };
        if candidates.is_empty() {
            return Ok(report);
        }
        let anchors = self.anchors()?;
        let until = instant_after(now, self.policy().cooling_window_secs);
        for fact in &candidates {
            report.facts_scanned += 1;
            if fact.cooled {
                continue;
            }
            let (Some(embedding), Some(model)) = (&fact.embedding, &fact.embedder_model) else {
                continue;
            };
            let Some(namespace_anchors) = anchors.get(&fact.namespace) else {
                continue;
            };
            let proximate = namespace_anchors.iter().find(|anchor| {
                anchor.model == *model
                    && is_core_proximate(
                        embedding.as_slice(),
                        anchor.embedding.as_slice(),
                        self.policy().core_proximity_threshold,
                    )
            });
            if let Some(anchor) = proximate {
                let audit = cooled_event(fact.id, &fact.namespace, anchor.block_id, &until, now);
                if self.store().cool_fact(fact.node, &until, &audit)? == CoolWrite::Applied {
                    report.facts_cooled += 1;
                }
            }
        }
        Ok(report)
    }

    /// The attested anchors, grouped by namespace: every live core block at or above
    /// the high-trust bar whose stored baseline parses. The anchor is the baseline's
    /// `block_embedding` — what the quorum co-signed — with its mandatory model
    /// identity for the cross-space guard.
    fn anchors(&self) -> Result<std::collections::HashMap<Namespace, Vec<Anchor>>, StoreError> {
        let mut anchors: std::collections::HashMap<Namespace, Vec<Anchor>> =
            std::collections::HashMap::new();
        for block in self.store().live_core_blocks()? {
            if block.stats.trust < self.policy().high_trust_threshold {
                continue;
            }
            let Some(stored) = &block.drift_baseline else {
                continue;
            };
            let Ok(baseline) = DriftBaseline::from_value(stored) else {
                continue;
            };
            anchors
                .entry(block.identity.namespace.clone())
                .or_default()
                .push(Anchor {
                    block_id: block.identity.id,
                    embedding: baseline.block_embedding,
                    model: baseline.embedder_model,
                });
        }
        Ok(anchors)
    }
}

/// The `Cooled` audit row for one applied stamp: a fresh transition id (idempotency
/// lives in the store's stamp-if-absent gate, the lifecycle-audit convention), the
/// fact's own namespace, and the window plus the anchoring block in the payload.
fn cooled_event(
    fact_id: Id,
    namespace: &Namespace,
    block_id: Id,
    until: &Timestamp,
    now: &Timestamp,
) -> AuditEvent {
    AuditEvent {
        identity: namespace_identity(transition_id(), namespace.clone(), now),
        kind: AuditKind::Cooled,
        subject_id: fact_id,
        actor_id: drift_actor(),
        payload: serde_json::json!({
            "reason": "core_proximity",
            "proximate_block": block_id.to_string(),
            "cooled_until": to_utc(until),
        }),
        signature: String::new(),
        occurred_at: now.clone(),
    }
}
