//! The drift detector (05 §1, M5.T05): per-namespace behavior centroid, per-block
//! assessment, and the skip taxonomy.
//!
//! Built by the engine only when the policy enables it — absent means off, the
//! [`Forgetter`](crate::Forgetter) pattern. The detector consumes **stored vectors
//! only**: episode embeddings frozen at write time and the attested baseline's
//! snapshots. It never calls an embedder, so there is no embedder-down condition on
//! the scoring path; what was never embedded is not measurable behavior and simply
//! drops from the sample.
//!
//! Skip-never-fabricate: every input the arithmetic cannot vouch for becomes a named
//! skip in the sweep report — missing or invalid baseline, foreign embedding space,
//! content moved since attestation, too-small sample — never a guessed score and
//! never a forced alarm. Forcing a maximum score on measurement failure would convert
//! every transient gap into an alarm storm, and an operator trained to ignore drift
//! warnings is the real security regression.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;

use aionforge_domain::blocks::Identity;
use aionforge_domain::drift::{behavior_centroid, crosses_threshold, drift_score};
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::core::CoreBlock;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::{Timestamp, instant_before, to_utc};
use aionforge_store::{Store, StoreError};

use crate::baseline::DriftBaseline;
use crate::policy::DriftPolicy;

/// The current behavior of one namespace, as far as the stored trace can vouch for
/// it: either a centroid over enough comparable episodes, or the reason there is no
/// centroid to compare against.
#[derive(Debug, Clone, PartialEq)]
pub enum CentroidOutcome {
    /// Enough comparable episodes; `sample_size` of them fed the centroid.
    Centroid {
        /// The normalized mean of the sample, in the live embedder's space.
        centroid: Embedding,
        /// How many episodes fed it.
        sample_size: usize,
    },
    /// Fewer comparable episodes than the policy floor (or a degenerate sample whose
    /// vectors carry no direction). Blocks in this namespace skip, never score.
    InsufficientSample {
        /// Comparable episodes found.
        have: usize,
        /// The policy floor.
        need: usize,
    },
}

/// Why a block needs a (re)baseline before it can ever be scored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaselineNeed {
    /// No baseline has ever been attested for this block.
    Missing,
    /// The block content moved (an attested edit) after the baseline was attested;
    /// the anchor no longer describes the block.
    ContentChanged,
}

/// One block's drift assessment: a score, or the named reason there is none.
#[derive(Debug, Clone, PartialEq)]
pub enum BlockAssessment {
    /// Scored against a real baseline and a real centroid.
    Scored {
        /// `clamp01(cos(baseline centroid, anchor) - cos(current centroid, anchor))`.
        score: f64,
        /// Whether the score crosses the policy threshold (a warning is due).
        crossed: bool,
        /// The baseline epoch the score was measured against — the anti-flap
        /// component of [`drift_warning_id`].
        baselined_at: Timestamp,
    },
    /// Baseline attested before the namespace had observed behavior
    /// (`behavior_centroid` is null): nothing to drift from, scores `0.0`, and the
    /// report names the block so an operator knows a rebaseline will arm it.
    AwaitingFirstBehavior,
    /// No usable baseline; the sweep report carries these as `baselines_needed`.
    NeedsBaseline(BaselineNeed),
    /// The baseline lives in a different embedding space than the live embedder
    /// (02 §13.5): never compared cross-space, reported so an attested rebaseline
    /// under the new model can be arranged. A model swap never auto-rebaselines —
    /// that would be laundering.
    StaleModel,
    /// The namespace sample sat below the policy floor.
    InsufficientSample {
        /// Comparable episodes found.
        have: usize,
        /// The policy floor.
        need: usize,
    },
    /// The stored `drift_baseline` JSON does not parse as a supported schema.
    InvalidBaseline {
        /// The parse or version failure, verbatim for the report.
        reason: String,
    },
}

/// The drift-detection orchestrator. Held by the engine as an `Option` — absent
/// means off and every drift facade surface is inert.
pub struct DriftDetector {
    store: Arc<Store>,
    policy: DriftPolicy,
}

impl DriftDetector {
    /// Build over the store with a validated policy.
    #[must_use]
    pub fn new(store: Arc<Store>, policy: DriftPolicy) -> Self {
        Self { store, policy }
    }

    /// The policy this detector runs.
    #[must_use]
    pub fn policy(&self) -> &DriftPolicy {
        &self.policy
    }

    /// The namespace's current behavior centroid over `[now - window, now)`,
    /// restricted to episodes embedded in the live model's space.
    ///
    /// Episodes under a different (or unrecorded) model drop out — after a model
    /// swap the namespace re-accumulates comparable behavior rather than mixing
    /// spaces — and a sample below the policy floor is a named skip, never a guess.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the episode read fails.
    pub fn behavior_centroid_now(
        &self,
        namespace: &Namespace,
        live_model: &EmbedderModel,
        now: &Timestamp,
    ) -> Result<CentroidOutcome, StoreError> {
        let since = instant_before(now, self.policy.behavior_window_secs);
        let sample = self.store.recent_embedded_episodes(
            namespace,
            &since,
            now,
            self.policy.behavior_sample_size,
        )?;
        let comparable: Vec<&[f32]> = sample
            .iter()
            .filter(|vector| vector.embedder_model.as_ref() == Some(live_model))
            .map(|vector| vector.embedding.as_slice())
            .collect();
        if comparable.len() < self.policy.min_sample_size {
            return Ok(CentroidOutcome::InsufficientSample {
                have: comparable.len(),
                need: self.policy.min_sample_size,
            });
        }
        match behavior_centroid(&comparable).and_then(|mean| Embedding::new(mean).ok()) {
            Some(centroid) => Ok(CentroidOutcome::Centroid {
                centroid,
                sample_size: comparable.len(),
            }),
            // A non-empty sample with no centroid means every vector was
            // direction-free (zero norm): nothing the arithmetic can vouch for.
            None => Ok(CentroidOutcome::InsufficientSample {
                have: 0,
                need: self.policy.min_sample_size,
            }),
        }
    }

    /// Assess one block against the namespace's current behavior. Pure — every
    /// input is already in hand — and ordered so the most actionable answer wins:
    /// a block whose baseline is missing, foreign, or stale reports that need even
    /// when the sample is also too small.
    #[must_use]
    pub fn assess_block(
        &self,
        block: &CoreBlock,
        live_model: &EmbedderModel,
        current: &CentroidOutcome,
    ) -> BlockAssessment {
        let Some(stored) = &block.drift_baseline else {
            return BlockAssessment::NeedsBaseline(BaselineNeed::Missing);
        };
        let baseline = match DriftBaseline::from_value(stored) {
            Ok(baseline) => baseline,
            Err(reason) => return BlockAssessment::InvalidBaseline { reason },
        };
        if baseline.embedder_model != *live_model {
            return BlockAssessment::StaleModel;
        }
        if !baseline.matches_content(block) {
            return BlockAssessment::NeedsBaseline(BaselineNeed::ContentChanged);
        }
        let Some(anchor_centroid) = &baseline.behavior_centroid else {
            return BlockAssessment::AwaitingFirstBehavior;
        };
        let centroid = match current {
            CentroidOutcome::Centroid { centroid, .. } => centroid,
            CentroidOutcome::InsufficientSample { have, need } => {
                return BlockAssessment::InsufficientSample {
                    have: *have,
                    need: *need,
                };
            }
        };
        let score = drift_score(
            anchor_centroid.as_slice(),
            baseline.block_embedding.as_slice(),
            centroid.as_slice(),
        );
        BlockAssessment::Scored {
            score,
            crossed: crosses_threshold(score, self.policy.drift_threshold),
            baselined_at: baseline.baselined_at,
        }
    }

    /// Sweep one page of live core blocks against the namespace behavior they anchor
    /// (05 §1): assess each, commit a [`AuditKind::DriftWarning`] row for every
    /// crossing, and tally. The audit log is the outbox — a warning row is
    /// content-addressed by [`drift_warning_id`], so re-sweeping the same drifting
    /// block against the same baseline epoch dedups to a no-op and
    /// [`DriftSweepReport::warnings_emitted`] reads back as zero.
    ///
    /// The page walks the **all-namespaces L0 spine** in ascending block-id order
    /// (drift is substrate maintenance, the forgetting sweep's convention); each
    /// warning is committed in the block's *own* namespace, agent-visible through the
    /// scoped audit reads. Each namespace's centroid is computed once per page.
    /// Detection never blocks a write and never mutates a block (05 §1) — the only
    /// writes here are audit rows.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the block enumeration, an episode read, or a warning
    /// commit fails.
    pub fn sweep(
        &self,
        live_model: &EmbedderModel,
        after: Option<&Id>,
        limit: usize,
        now: &Timestamp,
    ) -> Result<DriftSweepReport, StoreError> {
        if limit == 0 {
            return Ok(DriftSweepReport::default());
        }
        let mut blocks = self.store.live_core_blocks()?;
        blocks.sort_by_key(|block| block.identity.id);
        let page: Vec<&CoreBlock> = blocks
            .iter()
            .filter(|block| after.is_none_or(|cursor| block.identity.id > *cursor))
            .take(limit)
            .collect();
        let mut report = DriftSweepReport {
            next: page.last().map(|block| block.identity.id),
            ..DriftSweepReport::default()
        };
        let mut centroids: HashMap<Namespace, CentroidOutcome> = HashMap::new();
        for block in page {
            let namespace = &block.identity.namespace;
            let current = match centroids.entry(namespace.clone()) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    entry.insert(self.behavior_centroid_now(namespace, live_model, now)?)
                }
            };
            report.blocks_scanned += 1;
            match self.assess_block(block, live_model, current) {
                BlockAssessment::Scored {
                    score,
                    crossed,
                    baselined_at,
                } => {
                    report.max_score = Some(report.max_score.unwrap_or(0.0).max(score));
                    if crossed {
                        let sample_size = match current {
                            CentroidOutcome::Centroid { sample_size, .. } => *sample_size,
                            CentroidOutcome::InsufficientSample { .. } => 0,
                        };
                        let warning = warning_event(
                            block,
                            score,
                            self.policy.drift_threshold,
                            &baselined_at,
                            sample_size,
                            now,
                        );
                        let (_, created) = self.store.commit_audit_created(&warning)?;
                        if created {
                            report.warnings_emitted += 1;
                        }
                    }
                }
                BlockAssessment::AwaitingFirstBehavior => report.awaiting_first_behavior += 1,
                BlockAssessment::NeedsBaseline(_) => {
                    report.baselines_needed.push(block.identity.id);
                }
                BlockAssessment::StaleModel => report.blocks_stale_model += 1,
                BlockAssessment::InsufficientSample { .. }
                | BlockAssessment::InvalidBaseline { .. } => report.blocks_skipped += 1,
            }
        }
        Ok(report)
    }
}

/// One [`DriftDetector::sweep`] page's tally.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DriftSweepReport {
    /// Live blocks visited on this page; every block lands in exactly one of the
    /// buckets below or contributed to `max_score`.
    pub blocks_scanned: usize,
    /// Blocks whose baseline predates any observed behavior (genesis seed): scored
    /// `0.0`, armed only by an attested rebaseline once behavior exists.
    pub awaiting_first_behavior: usize,
    /// Blocks skipped for a too-small or degenerate behavior sample, or a stored
    /// baseline that does not parse.
    pub blocks_skipped: usize,
    /// Blocks whose baseline lives in a different embedding space than the live
    /// embedder — never compared, awaiting an attested rebaseline under the new
    /// model.
    pub blocks_stale_model: usize,
    /// Blocks with no usable baseline (never seeded, or content moved since
    /// attestation) — the actionable list for attesters to co-sign baselines over.
    pub baselines_needed: Vec<Id>,
    /// Warning rows **newly committed** on this page; replays of an already-warned
    /// `(block, baseline epoch, decile)` are excluded.
    pub warnings_emitted: usize,
    /// The highest score observed across scored blocks, for the operator's gauge.
    /// The crossing decision is always per-block, never on this aggregate.
    pub max_score: Option<f64>,
    /// The watermark to pass as `after` on the next call: the last block id this
    /// page visited, or `None` when the page was empty (the scan completed).
    /// Block ids are not time-ordered, so a recurring host must still start fresh
    /// (`after = None`) each full pass to see newly created blocks.
    pub next: Option<Id>,
}

/// The substrate actor recorded on drift warnings: detection runs at substrate
/// authority on the host's cadence and takes no principal, like the forgetter.
fn drift_actor() -> Id {
    Id::from_content_hash(b"aionforge/drift-detector-v1")
}

/// Build the warning row for one crossing: identified by [`drift_warning_id`]
/// (block × baseline epoch × score decile), committed in the block's own namespace
/// with the score's full context in the payload.
fn warning_event(
    block: &CoreBlock,
    score: f64,
    threshold: f64,
    baselined_at: &Timestamp,
    sample_size: usize,
    now: &Timestamp,
) -> AuditEvent {
    AuditEvent {
        identity: Identity {
            id: drift_warning_id(&block.identity.id, baselined_at, score),
            ingested_at: now.clone(),
            namespace: block.identity.namespace.clone(),
            expired_at: None,
        },
        kind: AuditKind::DriftWarning,
        subject_id: block.identity.id,
        actor_id: drift_actor(),
        payload: serde_json::json!({
            "block_kind": block.block_kind,
            "score": score,
            "threshold": threshold,
            "baselined_at": to_utc(baselined_at),
            "sample_size": sample_size,
        }),
        signature: String::new(),
        occurred_at: now.clone(),
    }
}

/// The content-addressed identity of a drift warning: one row per
/// `(block, baseline epoch, score decile)`, the anti-flap control. A re-detect of
/// the same drift against the same baseline dedups to a no-op through the audit
/// write's id idempotency; a rebaseline re-arms (new `baselined_at`), and an
/// escalation that climbs into a new decile warns once more — the operator sees the
/// trajectory without a row per sweep. The epoch component is the attestation
/// instant in UTC milliseconds, indifferent to zone representation.
#[must_use]
pub fn drift_warning_id(block_id: &Id, baselined_at: &Timestamp, score: f64) -> Id {
    let bucket = (score.clamp(0.0, 1.0) * 10.0).floor() as u8;
    let epoch_ms = to_utc(baselined_at).timestamp().as_millisecond();
    Id::from_content_hash(format!("drift_warning|{block_id}|{epoch_ms}|{bucket}").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_ids_dedup_within_an_epoch_bucket_and_rearm_on_rebaseline() {
        let block = Id::from_content_hash(b"block");
        let at: Timestamp = "2026-06-10T09:00:00-05:00[America/Chicago]"
            .parse()
            .expect("valid zoned datetime");
        let same_zone_other_repr = to_utc(&at);
        assert_eq!(
            drift_warning_id(&block, &at, 0.31),
            drift_warning_id(&block, &same_zone_other_repr, 0.39),
            "same block, epoch, and decile is one warning"
        );
        assert_ne!(
            drift_warning_id(&block, &at, 0.31),
            drift_warning_id(&block, &at, 0.45),
            "climbing a decile warns once more"
        );
        let rebaselined: Timestamp = "2026-06-11T09:00:00-05:00[America/Chicago]"
            .parse()
            .expect("valid zoned datetime");
        assert_ne!(
            drift_warning_id(&block, &at, 0.31),
            drift_warning_id(&block, &rebaselined, 0.31),
            "a rebaseline re-arms the warning"
        );
    }
}
