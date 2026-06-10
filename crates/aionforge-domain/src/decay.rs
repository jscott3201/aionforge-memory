//! Pure importance-decay primitives (05 §2, M5.T01).
//!
//! A memory's effective importance is a **derived value**: the stored
//! [`Stats::importance`](crate::blocks::Stats::importance) anchored at write time, sunk by
//! elapsed time since [`Stats::last_access`](crate::blocks::Stats::last_access) under a
//! per-tier exponential half-life.
//! Per §13.7 the substrate stores no authoritative copy of derived state, so the decayed
//! value is **never written back** — retrieval computes it at rank time, and the active
//! forgetting sweep (M5.T02) recomputes it at sweep time, both through these same pure
//! functions with a caller-supplied `now` (there is no ambient clock anywhere on either
//! path).
//!
//! Pinned memories never decay out of retrieval eligibility: [`decayed_importance`]
//! short-circuits on `is_pinned` and returns the stored importance untouched, and
//! [`is_eligible`] holds a pinned memory eligible regardless of any floor. The pin is a
//! plain branch on the [`Stats`](crate::blocks::Stats) scalar — it is never routed through a
//! loss-tolerant recompute.

/// The decay tier a memory kind belongs to (05 §2).
///
/// The spec names exactly two half-life classes: *short* for session-scoped episodic
/// memory, *long* for semantic and identity memory. Identity ([`CoreBlock`]) deliberately
/// folds into [`Tier::Semantic`] rather than carrying a third knob — the long class is one
/// half-life.
///
/// [`CoreBlock`]: crate::nodes::core::CoreBlock
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Session-scoped episodic memory; short half-life.
    Episodic,
    /// Semantic and identity memory; long half-life.
    Semantic,
}

/// The decay tier for a node label, or `None` for kinds that carry no
/// [`Stats`](crate::blocks::Stats) block (forensic, control, and agent kinds — nothing to
/// decay).
///
/// Keyed on the kinds' own `LABEL` constants so the mapping moves with a rename instead of
/// drifting from it. The seven `Stats`-bearing kinds are exactly the retrievable tables.
#[must_use]
pub fn tier_for_label(label: &str) -> Option<Tier> {
    use crate::nodes::associative::Note;
    use crate::nodes::core::CoreBlock;
    use crate::nodes::episodic::Episode;
    use crate::nodes::procedural::{BadPattern, Skill};
    use crate::nodes::semantic::{Entity, Fact};

    if label == Episode::LABEL {
        Some(Tier::Episodic)
    } else if label == Fact::LABEL
        || label == Entity::LABEL
        || label == Note::LABEL
        || label == Skill::LABEL
        || label == BadPattern::LABEL
        || label == CoreBlock::LABEL
    {
        Some(Tier::Semantic)
    } else {
        None
    }
}

/// The effective importance of a memory at `now`: the stored importance sunk by exponential
/// decay over the elapsed time since `last_access` (05 §2).
///
/// Pure and side-effect free — the result orders a ranking or feeds an eligibility check
/// and is never stored. Four deliberate short-circuits return the stored value unchanged:
///
/// - **Pinned.** A pinned memory never decays out of eligibility, so it keeps its full
///   write-time importance in every ranking.
/// - **Inert half-life.** A non-finite or non-positive `half_life_secs` means "no decay for
///   this tier" — the guard also keeps the division well-defined, so no configuration value
///   can produce a NaN.
/// - **Non-finite stored importance.** Garbage in is the same garbage out, never a *minted*
///   NaN: an infinite `stored` against an underflowed-to-zero factor would otherwise turn
///   into NaN, and NaN fails every `>=`, which would wrongly read as ineligible downstream.
/// - **Non-positive elapsed time.** A `last_access` at or ahead of `now` (clock regression,
///   or a future-stamped record) clamps to zero elapsed rather than *inflating* importance,
///   mirroring the consolidation lag clamp.
#[must_use]
pub fn decayed_importance(
    stored: f64,
    last_access: &crate::time::Timestamp,
    now: &crate::time::Timestamp,
    half_life_secs: f64,
    is_pinned: bool,
) -> f64 {
    if is_pinned || !stored.is_finite() || !half_life_secs.is_finite() || half_life_secs <= 0.0 {
        return stored;
    }
    let elapsed = (now.timestamp().as_second() - last_access.timestamp().as_second()).max(0);
    if elapsed == 0 {
        return stored;
    }
    // Whole seconds are ample resolution for half-lives measured in days, and the
    // instant-based difference is robust across time-zone representations.
    #[allow(clippy::cast_precision_loss)]
    let halvings = elapsed as f64 / half_life_secs;
    stored * 0.5_f64.powf(halvings)
}

/// Whether a memory remains eligible at a decayed importance against `floor`.
///
/// The single shared definition of the pin-protection rule: a pinned memory is eligible
/// no matter how far its unpinned peers would have decayed. M5.T01's retrieval re-rank
/// never drops by importance, so it does not consult this; it is the seam the M5.T02
/// soft-expire sweep calls with its own configured floor.
#[must_use]
pub fn is_eligible(is_pinned: bool, decayed: f64, floor: f64) -> bool {
    is_pinned || decayed >= floor
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::Timestamp;

    const EPS: f64 = 1e-12;
    const HOUR: f64 = 3_600.0;

    fn at(hour: u32) -> Timestamp {
        format!("2026-06-09T{hour:02}:00:00-05:00[America/Chicago]")
            .parse()
            .expect("valid zoned datetime")
    }

    #[test]
    fn importance_halves_at_each_half_life() {
        let stored = 0.8;
        let one = decayed_importance(stored, &at(0), &at(1), HOUR, false);
        let two = decayed_importance(stored, &at(0), &at(2), HOUR, false);
        assert!((one - 0.4).abs() < EPS, "one half-life halves: {one}");
        assert!((two - 0.2).abs() < EPS, "two half-lives quarter: {two}");
    }

    #[test]
    fn decay_is_monotonic_in_elapsed_time() {
        let stored = 0.5;
        let mut previous = stored;
        for hour in 1..=12 {
            let decayed = decayed_importance(stored, &at(0), &at(hour), 5.0 * HOUR, false);
            assert!(
                decayed < previous,
                "hour {hour}: {decayed} must sink below {previous}"
            );
            assert!(decayed > 0.0, "exponential decay never reaches zero");
            previous = decayed;
        }
    }

    #[test]
    fn zero_and_negative_elapsed_return_the_stored_value() {
        let stored = 0.7;
        let same = decayed_importance(stored, &at(3), &at(3), HOUR, false);
        assert!((same - stored).abs() < EPS, "zero elapsed is no decay");
        // A future-stamped last_access (clock regression) clamps to zero elapsed —
        // importance is returned unchanged, never inflated.
        let regressed = decayed_importance(stored, &at(9), &at(3), HOUR, false);
        assert!((regressed - stored).abs() < EPS, "negative elapsed clamps");
    }

    #[test]
    fn a_pinned_memory_never_decays() {
        let stored = 0.6;
        let decayed = decayed_importance(stored, &at(0), &at(12), HOUR, true);
        assert!(
            (decayed - stored).abs() < EPS,
            "the pin short-circuits every elapsed time"
        );
    }

    #[test]
    fn an_inert_half_life_returns_the_stored_value() {
        let stored = 0.9;
        for half_life in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let decayed = decayed_importance(stored, &at(0), &at(12), half_life, false);
            assert!(
                (decayed - stored).abs() < EPS,
                "half-life {half_life} must be inert, got {decayed}"
            );
        }
    }

    #[test]
    fn a_non_finite_stored_importance_passes_through_unminted() {
        // Garbage in, same garbage out: an infinite stored value against a tiny decay
        // factor must come back as itself, never as a NaN the arithmetic minted (an
        // infinite stored times an underflowed-to-zero factor is NaN, and NaN fails
        // every `>=`, so a minted NaN would wrongly read as ineligible downstream).
        for stored in [f64::INFINITY, f64::NEG_INFINITY] {
            let decayed = decayed_importance(stored, &at(0), &at(12), 1e-6, false);
            assert_eq!(decayed, stored, "{stored} passes through unchanged");
        }
        let nan = decayed_importance(f64::NAN, &at(0), &at(12), HOUR, false);
        assert!(nan.is_nan(), "a NaN input stays NaN, it is not laundered");
    }

    #[test]
    fn tiers_map_from_the_label_constants() {
        use crate::nodes::associative::Note;
        use crate::nodes::core::CoreBlock;
        use crate::nodes::episodic::Episode;
        use crate::nodes::procedural::{BadPattern, Skill};
        use crate::nodes::semantic::{Entity, Fact};

        assert_eq!(tier_for_label(Episode::LABEL), Some(Tier::Episodic));
        for label in [
            Fact::LABEL,
            Entity::LABEL,
            Note::LABEL,
            Skill::LABEL,
            BadPattern::LABEL,
            CoreBlock::LABEL,
        ] {
            assert_eq!(tier_for_label(label), Some(Tier::Semantic), "{label}");
        }
        // Stats-less kinds carry nothing to decay.
        for label in ["AuditEvent", "Agent", "ConsolidationCursor", "NoSuchKind"] {
            assert_eq!(tier_for_label(label), None, "{label}");
        }
    }

    #[test]
    fn eligibility_holds_pinned_memories_above_any_floor() {
        assert!(is_eligible(true, 0.0, 0.9), "a pin overrides the floor");
        assert!(is_eligible(false, 0.5, 0.5), "at the floor is eligible");
        assert!(!is_eligible(false, 0.49, 0.5), "below the floor is not");
    }
}
