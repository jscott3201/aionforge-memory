//! The pluggable consolidation pass seam (write-and-consolidation §2).
//!
//! A pass is one rule the consolidator applies to an episode — extract facts, resolve
//! entities, detect supersession, summarize (M2.T04–T06). M2.T03 ships only the seam
//! and a [`NoopPass`] to prove the machinery; the real rules slot in here without
//! touching the scheduler.
//!
//! The contract is deliberately read-only: a pass reads a snapshot and returns derived
//! output; it must **not** open a write transaction. The scheduler commits the result
//! atomically with the episode's state-flip, which is what lets a crash mid-pass resume
//! without double-applying — and structurally honors the rule that consolidation never
//! re-enters the write path from inside a callback.

use std::sync::Arc;

use aionforge_domain::nodes::episodic::Episode;
use aionforge_domain::time::Timestamp;
use aionforge_store::{NodeId, Store};

use crate::profile::PassProfile;

/// What a pass derived for the scheduler to commit.
///
/// This is the store's [`ConsolidationArtifacts`](aionforge_store::ConsolidationArtifacts)
/// payload — the entities, facts, and edges the scheduler materializes atomically with
/// the episode flip. It is re-exported under this name because "pass output" is the seam
/// vocabulary, while "consolidation artifacts" is the storage vocabulary; they are one
/// type so a pass's return value is exactly what the commit writes, with no copy step.
/// A no-deriving pass returns [`PassOutput::default`]; M2.T04+ passes fill it in.
pub use aionforge_store::ConsolidationArtifacts as PassOutput;

/// What a pass returns: the artifacts to commit plus a content-free per-stage profile.
///
/// The artifacts ([`PassOutput`]) are what the scheduler materializes; the
/// [`PassProfile`] is the operator-facing counts/outcomes the scheduler accumulates into the
/// tick's [`ConsolidationProfile`](crate::ConsolidationProfile) for a verbose receipt. The
/// profile is bundled here, rather than added to [`PassOutput`], so the store crate's
/// [`ConsolidationArtifacts`](aionforge_store::ConsolidationArtifacts) stays a pure storage
/// payload (the store crate boundary carries no profiling vocabulary).
pub struct PassRun {
    /// The derived artifacts for the scheduler to commit.
    pub output: PassOutput,
    /// The per-stage profile (counts/outcomes only) for the verbose receipt.
    pub profile: PassProfile,
}

impl PassRun {
    /// A run that derived `output` and carries no profiled stage (the seam-only/no-op case).
    #[must_use]
    pub fn unprofiled(output: PassOutput) -> Self {
        Self {
            output,
            profile: PassProfile::empty(),
        }
    }

    /// A run that derived nothing and carries no profile (the empty no-op case).
    #[must_use]
    pub fn empty() -> Self {
        Self::unprofiled(PassOutput::default())
    }
}

/// One consolidation rule over a single episode.
#[async_trait::async_trait]
pub trait ConsolidationPass: Send + Sync + 'static {
    /// A stable, unique name (e.g. `"extract_facts"`). Keys the cursor's `rule_versions`.
    fn name(&self) -> &'static str;

    /// A monotonic version. A bump signals "reprocess" to later milestones; M2.T03
    /// records it only.
    fn version(&self) -> u32;

    /// Whether this pass is enabled (a config/feature gate). Disabled passes are skipped
    /// and excluded from `rule_versions`.
    fn enabled(&self) -> bool {
        true
    }

    /// Derive output for one episode from a read-only snapshot. Side-effect-free: the
    /// scheduler, not the pass, performs every write.
    ///
    /// Returns a [`PassRun`] bundling the artifacts to commit with a content-free per-stage
    /// [`PassProfile`]; a pass with nothing to report returns [`PassRun::unprofiled`].
    ///
    /// # Errors
    /// Returns [`PassError::Transient`] for a recoverable failure (retry next tick) or
    /// [`PassError::Fatal`] for a permanent one (the episode is marked failed).
    async fn apply(&self, cx: &PassContext<'_>) -> Result<PassRun, PassError>;
}

/// The read-only context handed to a pass for one episode.
pub struct PassContext<'a> {
    /// The store, for snapshot reads only. A pass must not open a write transaction.
    pub store: &'a Arc<Store>,
    /// The episode's engine node id (for the scheduler's flip; passes rarely need it).
    pub episode_node_id: NodeId,
    /// The episode under consolidation.
    pub episode: &'a Episode,
    /// The injected action time (`now`) for any time-stamped derivation.
    pub now: Timestamp,
    /// The `{pass_name: version}` set in force at this cursor position.
    pub rule_versions: &'a serde_json::Value,
}

/// A pass-level failure, classified for the scheduler's retry/halt decision.
#[derive(Debug, thiserror::Error)]
pub enum PassError {
    /// Recoverable (inference unavailable, rate limit, timeout): retry next tick.
    #[error("transient: {0}")]
    Transient(String),
    /// Permanent (bad input, invariant violation): mark the episode failed.
    #[error("fatal: {0}")]
    Fatal(String),
}

/// The no-op pass: it derives nothing and always succeeds.
///
/// It exists so M2.T03 can prove the scheduler end to end — discover, apply, flip,
/// advance the cursor — before any real rule exists. It is not registered in production.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopPass;

#[async_trait::async_trait]
impl ConsolidationPass for NoopPass {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn version(&self) -> u32 {
        1
    }

    async fn apply(&self, _cx: &PassContext<'_>) -> Result<PassRun, PassError> {
        Ok(PassRun::empty())
    }
}
