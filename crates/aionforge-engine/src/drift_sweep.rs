//! The drift-detection facade: the off-cursor sweep and the baseline-computation
//! helper (05 §1, M5.T05).
//!
//! Split out of `lib.rs` like its sweep siblings. Everything here is **off-cursor**
//! and host-cadence — the host calls on its own schedule with its own clock — and the
//! `Option<DriftDetector>` is the single off-switch: absent, the sweep returns an
//! empty report without touching the graph and the baseline helper answers
//! [`BaselineComputation::Disabled`].
//!
//! Both surfaces run at **substrate authority** and take no principal, the
//! forgetting-sweep convention: drift is maintenance over the whole identity tier,
//! and a scoped scan would silently skip another namespace's drifting blocks. The
//! host gates who may call them and who may read the resulting `drift_warning` rows
//! (the scoped audit facade filters by namespace; each warning is committed in its
//! block's own namespace).

use aionforge_domain::contracts::Embedder;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::time::{Timestamp, to_utc};
use aionforge_forget::{CentroidOutcome, DriftBaseline, DriftSweepReport};

use crate::{EngineError, Memory};

/// The outcome of [`Memory::compute_drift_baseline`]: the document for attesters to
/// co-sign, or the named reason there is none. Never an auto-applied write — the
/// returned baseline becomes real only through the attested core-edit path, because
/// the baseline is the asset drift detection guards and an un-attested write to it
/// is the laundering primitive.
#[derive(Debug, Clone, PartialEq)]
pub enum BaselineComputation {
    /// Drift detection is off; no detector is constructed.
    Disabled,
    /// No live core block has this id (never created, or retired).
    NotFound,
    /// The proposed baseline, computed from the block's current content under the
    /// live embedder and the namespace's current behavior window.
    Computed(DriftBaseline),
}

impl<E: Embedder> Memory<E> {
    /// Sweep one page of live core blocks for identity drift (05 §1): score each
    /// against its attested baseline, commit a `drift_warning` audit row per
    /// crossing, and tally every named skip. Detection never blocks a write and
    /// never mutates a block; re-sweeping warned ground dedups through the
    /// content-addressed warning id and reads back as zero
    /// [`DriftSweepReport::warnings_emitted`].
    ///
    /// **Host-driven cadence**, like [`Memory::sweep_forgetting`]: call on a timer
    /// with your own clock, pass [`DriftSweepReport::next`] back as `after` to
    /// continue a page walk, and start fresh (`after = None`) each full pass —
    /// block ids are not time-ordered, so only a fresh pass sees new blocks. The
    /// report's `baselines_needed` is the actionable list: those blocks score
    /// nothing until attesters co-sign a baseline through the core-edit path.
    ///
    /// # Errors
    /// Returns [`EngineError::Store`] if the block enumeration, an episode read, or
    /// a warning commit fails.
    pub fn sweep_drift(
        &self,
        after: Option<&Id>,
        limit: usize,
        now: &Timestamp,
    ) -> Result<DriftSweepReport, EngineError> {
        let Some(detector) = &self.drift_detector else {
            return Ok(DriftSweepReport::default());
        };
        Ok(detector.sweep(self.embedder.model(), after, limit, now)?)
    }

    /// Compute the drift-baseline document for one live core block: the block's
    /// current content embedded under the live embedder, the namespace's current
    /// behavior centroid, and the integrity anchors (05 §1). The one drift surface
    /// that calls the embedder — scoring never does.
    ///
    /// The result is a **proposal**: feed it into the attested core-edit draft
    /// (content unchanged, `drift_baseline` replaced) for the quorum to co-sign.
    /// A namespace with too little embedded behavior yields a genesis baseline
    /// (`behavior_centroid: None`, `sample_size: 0`) — attestable now, scoring
    /// `0.0` until a rebaseline captures real behavior, reported as
    /// awaiting-first-behavior by the sweep.
    ///
    /// # Errors
    /// Returns [`EngineError::Store`] if the block or episode read fails, and
    /// [`EngineError::Drift`] if embedding the block content fails or returns the
    /// wrong vector count.
    pub async fn compute_drift_baseline(
        &self,
        block_id: &Id,
        now: &Timestamp,
    ) -> Result<BaselineComputation, EngineError> {
        let Some(detector) = &self.drift_detector else {
            return Ok(BaselineComputation::Disabled);
        };
        let Some(block) = self.store.core_block_by_id(block_id)? else {
            return Ok(BaselineComputation::NotFound);
        };
        if block.identity.expired_at.is_some() {
            // A retired block is out of the detector's enumeration; a baseline for
            // it would be dead state the moment it was attested.
            return Ok(BaselineComputation::NotFound);
        }
        let mut vectors = self
            .embedder
            .embed(std::slice::from_ref(&block.content))
            .await
            .map_err(|error| {
                EngineError::Drift(format!("embedding the block content failed: {error}"))
            })?;
        if vectors.len() != 1 {
            return Err(EngineError::Drift(format!(
                "embedding the block content returned {} vectors for one input",
                vectors.len()
            )));
        }
        let block_embedding = vectors.remove(0);
        let live_model = self.embedder.model();
        let (behavior_centroid, sample_size) =
            match detector.behavior_centroid_now(&block.identity.namespace, live_model, now)? {
                CentroidOutcome::Centroid {
                    centroid,
                    sample_size,
                } => (Some(centroid), sample_size),
                CentroidOutcome::InsufficientSample { .. } => (None, 0),
            };
        Ok(BaselineComputation::Computed(DriftBaseline {
            v: DriftBaseline::VERSION,
            embedder_model: live_model.clone(),
            content_hash: ContentHash::of(block.content.as_bytes()),
            block_embedding,
            behavior_centroid,
            baselined_at: to_utc(now),
            window_secs: detector.policy().behavior_window_secs,
            sample_size,
        }))
    }
}
