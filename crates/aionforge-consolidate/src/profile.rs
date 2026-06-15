//! Per-stage consolidation profile: deterministic counts/outcomes only (Task #15).
//!
//! When `consolidate(verbose=true)` runs, an operator needs a machine-checkable answer to
//! "why did 0 notes appear?" — was the stage disabled, did it see no candidates, or did a
//! guard reject everything it saw? This module models that answer as a small, content-free
//! [`StageProfile`] per logical stage, a [`PassProfile`] (one pass's stages), and a
//! [`ConsolidationProfile`] that sums profiles across the passes of a tick (and across
//! ticks of a foreground run) in a fixed canonical stage order.
//!
//! **Stages, not passes.** Production registers exactly two [`ConsolidationPass`] impls —
//! `FactExtractionPass` (which internally performs the resolution, detection, and
//! summarization work) and `SkillInductionPass` (induction). The four *stage* names are the
//! operator-facing decomposition of that work, owner-approved as stages rather than four
//! separate passes, so the profile reads the same whether a stage lives in its own pass or
//! is folded into a larger one.
//!
//! **Counts only.** Every field here is a `u64` count or a `bool` gate. The profile never
//! carries content, ids, embeddings, tokens, statements, or arguments — it is safe to write
//! verbatim into a tool receipt or a span. The merge is a deterministic sum over a fixed
//! stage order, so a replay over the same episodes yields a byte-identical profile.
//!
//! [`ConsolidationPass`]: crate::ConsolidationPass

/// The canonical stage names, in their fixed display/merge order.
///
/// The order is the consolidation data-flow order — resolution feeds detection feeds
/// summarization; induction is independent — and it is the order [`ConsolidationProfile`]
/// renders and merges in, so a profile is deterministic regardless of pass registration
/// order. Every stage a pass reports must be one of these names; an unknown name is summed
/// into its own slot (so a future stage cannot silently vanish) but ordered after the known
/// stages.
pub const STAGE_ORDER: [&str; 4] = [
    STAGE_RESOLUTION,
    STAGE_DETECTION,
    STAGE_SUMMARIZATION,
    STAGE_INDUCTION,
];

/// The entity-resolution stage: surface forms resolved to canonical entities.
pub const STAGE_RESOLUTION: &str = "resolution";
/// The supersession/contradiction detection stage.
pub const STAGE_DETECTION: &str = "detection";
/// The summary-note stage: fact clusters condensed into notes.
pub const STAGE_SUMMARIZATION: &str = "summarization";
/// The skill-induction stage: a recurring episode induced into a skill.
pub const STAGE_INDUCTION: &str = "induction";

/// One logical consolidation stage's outcome for one episode (or summed across many).
///
/// Every field is a content-free count or gate, so a [`StageProfile`] is safe to render into
/// a tool receipt or a span. The fields distinguish the three "0 derived" cases an operator
/// asks about: a disabled stage (`enabled == false`), a stage that saw nothing
/// (`candidates_considered == 0`), and a stage whose candidates were all turned away
/// (`candidates_considered > 0` but `derived == 0`, with the rejections counted in
/// [`quarantined`](Self::quarantined) and [`rejected_by_guard`](Self::rejected_by_guard)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageProfile {
    /// The canonical stage name (one of [`STAGE_ORDER`]).
    pub stage: &'static str,
    /// Whether the stage ran at all this tick. A disabled stage is reported with a `false`
    /// gate and zero counts, so "disabled" is distinguishable from "ran, saw nothing".
    pub enabled: bool,
    /// How many candidates the stage examined (surfaces, facts, clusters, or the single
    /// induction candidate). Zero with `enabled == true` means the stage ran but had no input.
    pub candidates_considered: u64,
    /// How many outputs the stage produced (entities, supersessions, notes, induced skills).
    pub derived: u64,
    /// How many candidates the stage merged into an existing record rather than deriving a
    /// new one (resolution surfaces that matched a committed entity). Zero for stages with no
    /// merge concept.
    pub merged: u64,
    /// How many candidates the stage quarantined (detection contradictions flagged for
    /// review). Zero for stages with no quarantine concept.
    pub quarantined: u64,
    /// How many candidates a conservative guard rejected (the detail-retention guard skipping
    /// an over-lossy summary; induction's reuse/structure gates declining an episode). This is
    /// the count that explains a "saw candidates, derived nothing" outcome.
    pub rejected_by_guard: u64,
}

impl StageProfile {
    /// A stage that did not run: the gate is `false` and every count is zero.
    ///
    /// Used to report a config-disabled stage (detection/summarization off, induction not
    /// opted in) so its slot is present in the profile rather than missing — "disabled" is an
    /// answer, not an absence.
    #[must_use]
    pub fn disabled(stage: &'static str) -> Self {
        Self {
            stage,
            enabled: false,
            candidates_considered: 0,
            derived: 0,
            merged: 0,
            quarantined: 0,
            rejected_by_guard: 0,
        }
    }

    /// An enabled stage with explicit counts.
    #[must_use]
    pub fn enabled(
        stage: &'static str,
        candidates_considered: u64,
        derived: u64,
        merged: u64,
        quarantined: u64,
        rejected_by_guard: u64,
    ) -> Self {
        Self {
            stage,
            enabled: true,
            candidates_considered,
            derived,
            merged,
            quarantined,
            rejected_by_guard,
        }
    }

    /// Fold another profile for the same stage into this one: OR the gate, sum the counts.
    ///
    /// The gate is OR-ed so that if the stage ran in any folded tick it reads as enabled;
    /// the counts sum so the result is the total work across every folded tick/pass.
    fn fold(&mut self, other: &StageProfile) {
        self.enabled |= other.enabled;
        self.candidates_considered += other.candidates_considered;
        self.derived += other.derived;
        self.merged += other.merged;
        self.quarantined += other.quarantined;
        self.rejected_by_guard += other.rejected_by_guard;
    }
}

/// One [`ConsolidationPass`]'s stage profiles for one episode.
///
/// A pass reports a [`StageProfile`] for each logical stage it performs: `FactExtractionPass`
/// reports resolution, detection, and summarization; `SkillInductionPass` reports induction.
/// The order of the inner vector is the pass's own; [`ConsolidationProfile`] re-orders into
/// [`STAGE_ORDER`] when it folds them, so determinism does not depend on pass authoring order.
///
/// [`ConsolidationPass`]: crate::ConsolidationPass
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PassProfile {
    /// The per-stage profiles this pass produced (in the pass's own order).
    pub stages: Vec<StageProfile>,
}

impl PassProfile {
    /// An empty profile: a pass that performed no profiled stage this tick.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// A profile from an explicit list of stage profiles.
    #[must_use]
    pub fn from_stages(stages: Vec<StageProfile>) -> Self {
        Self { stages }
    }
}

/// A consolidation profile accumulated across the passes of a tick and the ticks of a run.
///
/// Built empty, then [`merge`](Self::merge)d with each [`PassProfile`] a pass returns and
/// each per-tick profile a foreground run produces. The accumulation is a deterministic sum
/// in [`STAGE_ORDER`]: folding the same set of profiles in any order yields the same result,
/// so a replay over the same episodes produces a byte-identical profile (replay-safety).
///
/// The stages are stored in a fixed-capacity, canonically-ordered vector keyed by stage name,
/// so [`stages`](Self::stages) always returns the known stages first in [`STAGE_ORDER`] and
/// any unknown stage after them in first-seen order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConsolidationProfile {
    /// The accumulated per-stage profiles, canonically ordered (see [`Self::stages`]).
    stages: Vec<StageProfile>,
}

impl ConsolidationProfile {
    /// An empty profile (no stage seen yet).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether no stage has been folded in yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }

    /// Fold a pass's profile in: each stage is summed into its canonical slot.
    ///
    /// A known stage (one of [`STAGE_ORDER`]) lands in its fixed-order slot; an unknown stage
    /// is appended after the known ones in first-seen order. Folding is commutative and
    /// associative over the per-stage fold (OR the gate, sum the counts), so the merged result
    /// does not depend on the order passes or ticks are folded in.
    pub fn merge(&mut self, pass: &PassProfile) {
        for stage in &pass.stages {
            self.fold_stage(stage);
        }
    }

    /// Fold another accumulated profile in: the per-tick accumulator the scheduler folds an
    /// episode's profile into, and the per-run accumulator a foreground caller folds each
    /// tick's profile into. Like [`merge`](Self::merge), it sums each stage into its canonical
    /// slot, so the result is order-independent and replay-safe.
    pub fn merge_profile(&mut self, other: &ConsolidationProfile) {
        for stage in &other.stages {
            self.fold_stage(stage);
        }
    }

    /// Fold a single stage profile into its canonical slot, creating it if absent.
    fn fold_stage(&mut self, incoming: &StageProfile) {
        if let Some(existing) = self.stages.iter_mut().find(|s| s.stage == incoming.stage) {
            existing.fold(incoming);
        } else {
            self.stages.push(*incoming);
            self.reorder();
        }
    }

    /// Re-sort the stages into canonical order: known stages first in [`STAGE_ORDER`], then
    /// any unknown stage in its current (first-seen) relative order. A stable sort keyed on
    /// each stage's rank keeps unknown stages in insertion order.
    fn reorder(&mut self) {
        self.stages.sort_by_key(|s| stage_rank(s.stage));
    }

    /// The accumulated stages in canonical order ([`STAGE_ORDER`] first, then unknown stages).
    #[must_use]
    pub fn stages(&self) -> &[StageProfile] {
        &self.stages
    }
}

/// The canonical rank of a stage name: its index in [`STAGE_ORDER`], or a value past the end
/// for an unknown stage (so unknown stages sort after the known ones, ties broken by the
/// stable sort into first-seen order).
fn stage_rank(stage: &str) -> usize {
    STAGE_ORDER
        .iter()
        .position(|known| *known == stage)
        .unwrap_or(STAGE_ORDER.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(stages: Vec<StageProfile>) -> PassProfile {
        PassProfile::from_stages(stages)
    }

    #[test]
    fn merge_sums_counts_and_ors_the_gate() {
        let mut acc = ConsolidationProfile::new();
        acc.merge(&profile(vec![StageProfile::enabled(
            STAGE_DETECTION,
            2,
            1,
            0,
            1,
            0,
        )]));
        acc.merge(&profile(vec![StageProfile::enabled(
            STAGE_DETECTION,
            3,
            0,
            0,
            0,
            2,
        )]));

        let detection = acc
            .stages()
            .iter()
            .find(|s| s.stage == STAGE_DETECTION)
            .expect("detection stage present");
        assert!(detection.enabled, "the gate ORs to enabled");
        assert_eq!(detection.candidates_considered, 5, "candidates sum");
        assert_eq!(detection.derived, 1, "derived sum");
        assert_eq!(detection.quarantined, 1, "quarantined sum");
        assert_eq!(detection.rejected_by_guard, 2, "rejected sum");
    }

    #[test]
    fn merge_is_order_independent_and_canonically_ordered() {
        // Two passes' worth of stages folded in two different orders must produce an
        // identical profile (replay-safety), and always in STAGE_ORDER.
        let extraction = profile(vec![
            StageProfile::enabled(STAGE_RESOLUTION, 4, 2, 2, 0, 0),
            StageProfile::enabled(STAGE_DETECTION, 1, 0, 0, 0, 0),
            StageProfile::disabled(STAGE_SUMMARIZATION),
        ]);
        let induction = profile(vec![StageProfile::enabled(STAGE_INDUCTION, 1, 0, 0, 0, 1)]);

        let mut forward = ConsolidationProfile::new();
        forward.merge(&extraction);
        forward.merge(&induction);

        let mut reversed = ConsolidationProfile::new();
        reversed.merge(&induction);
        reversed.merge(&extraction);

        assert_eq!(forward, reversed, "merge is order-independent");
        let names: Vec<&str> = forward.stages().iter().map(|s| s.stage).collect();
        assert_eq!(
            names,
            vec![
                STAGE_RESOLUTION,
                STAGE_DETECTION,
                STAGE_SUMMARIZATION,
                STAGE_INDUCTION
            ],
            "stages render in canonical STAGE_ORDER regardless of fold order"
        );
    }

    #[test]
    fn disabled_distinguishes_from_ran_but_empty_and_from_rejected() {
        // The three "derived 0" cases an operator must tell apart.
        let disabled = StageProfile::disabled(STAGE_SUMMARIZATION);
        assert!(!disabled.enabled);
        assert_eq!(disabled.candidates_considered, 0);

        let ran_empty = StageProfile::enabled(STAGE_SUMMARIZATION, 0, 0, 0, 0, 0);
        assert!(ran_empty.enabled);
        assert_eq!(ran_empty.candidates_considered, 0);

        let rejected = StageProfile::enabled(STAGE_SUMMARIZATION, 3, 0, 0, 0, 3);
        assert!(rejected.enabled);
        assert_eq!(rejected.candidates_considered, 3, "it saw candidates");
        assert_eq!(rejected.derived, 0, "but derived nothing");
        assert_eq!(
            rejected.rejected_by_guard, 3,
            "because the guard rejected them"
        );

        assert_ne!(disabled, ran_empty, "disabled differs from ran-but-empty");
        assert_ne!(ran_empty, rejected, "ran-but-empty differs from rejected");
    }

    #[test]
    fn unknown_stage_sorts_after_known_stages_in_first_seen_order() {
        let mut acc = ConsolidationProfile::new();
        acc.merge(&profile(vec![
            StageProfile::enabled("zeta_future", 1, 0, 0, 0, 0),
            StageProfile::enabled(STAGE_RESOLUTION, 1, 1, 0, 0, 0),
            StageProfile::enabled("alpha_future", 1, 0, 0, 0, 0),
        ]));
        let names: Vec<&str> = acc.stages().iter().map(|s| s.stage).collect();
        assert_eq!(
            names,
            vec![STAGE_RESOLUTION, "zeta_future", "alpha_future"],
            "known stage first, unknown stages after in first-seen order"
        );
    }

    #[test]
    fn empty_profile_folds_into_a_no_op() {
        let mut acc = ConsolidationProfile::new();
        acc.merge(&PassProfile::empty());
        assert!(
            acc.is_empty(),
            "folding an empty pass profile changes nothing"
        );
    }
}
