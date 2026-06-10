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

use std::sync::Arc;

use aionforge_domain::drift::{behavior_centroid, crosses_threshold, drift_score};
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::core::CoreBlock;
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
        }
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
