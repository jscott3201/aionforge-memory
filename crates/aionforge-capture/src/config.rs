//! Capture-path tuning knobs (04 §1).

/// Tuning for the capture path.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureConfig {
    /// Whether to embed content on the capture path. When `false`, episodes are
    /// written without a vector and embedded lazily during consolidation (04 §1).
    pub embed_on_capture: bool,
    /// The cosine-*similarity* threshold above which a new episode counts as a
    /// near-duplicate of an existing one (04 §1 step 2). In `[0, 1]`; a value of
    /// `0.95` flags anything within cosine distance `0.05`. Near-duplicate episodes
    /// are still written — episodes are immutable and append-only — but flagged on
    /// the receipt so consolidation can cluster or summarize them.
    pub near_duplicate_threshold: f64,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            embed_on_capture: true,
            near_duplicate_threshold: 0.95,
        }
    }
}
