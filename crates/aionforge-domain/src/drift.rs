//! Pure drift-detection primitives (05 §1, M5.T05).
//!
//! Identity drift is a **derived value**: a comparison between where recent agent
//! behavior sits relative to an attested core block and where it sat when the block's
//! `drift_baseline` was taken. Per §13.7 the substrate stores no authoritative copy of
//! derived state, so nothing here is ever written back — the drift sweep computes the
//! score at sweep time and the cooling modulation is applied at rank time, both
//! through these same pure functions with a caller-supplied `now` (no ambient clock
//! anywhere on either path), mirroring [`decay`](crate::decay).
//!
//! The functions consume **stored** vectors only. The detector never calls the
//! embedder: a memory without an embedding simply drops out of the sample, and an
//! empty sample is the caller's skip condition, never a fabricated score — degrade,
//! don't block (03 §8.1).
//!
//! Cooling follows the same conservative-guard discipline as the forget axes: garbage
//! in is the same garbage out, and a value the arithmetic cannot vouch for never
//! *reduces* a trust scalar — the modulation spares, it never dooms.

use crate::time::Timestamp;

/// Cosine similarity over two stored vectors, clamped to `[0, 1]` — the house
/// similarity primitive (every vector index in the substrate is cosine). A length
/// mismatch or a zero-norm side answers `0.0`: "no measurable similarity", never an
/// error, so a degenerate vector cannot poison a score. Accumulates in `f64`, so a
/// non-finite component yields a non-finite ratio that the clamp pins to a bound
/// rather than letting `NaN` escape (`NaN.clamp(0,1)` is `NaN`, but the norm guards
/// catch the zero cases and the caller's finiteness checks own the rest).
#[must_use]
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0_f64;
    let mut norm_a = 0.0_f64;
    let mut norm_b = 0.0_f64;
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 || !norm_a.is_finite() || !norm_b.is_finite() {
        return 0.0;
    }
    let similarity = dot / (norm_a.sqrt() * norm_b.sqrt());
    if similarity.is_finite() {
        similarity.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// The normalized centroid of a behavior sample: each vector is normalized, the
/// normalized vectors are averaged component-wise, and the mean is re-normalized —
/// so one long vector cannot dominate the direction, and the result is itself a unit
/// vector ready for [`cosine`].
///
/// Returns `None` for an empty sample, mismatched dimensions, or a zero-norm mean
/// (behavior pointing every which way cancels out): the caller's *skip* condition.
/// The detector never scores what it cannot measure. **Input order is the caller's
/// canonical order** (`(ingested_at, id)` for episodes); the mean is
/// order-insensitive mathematically, but float summation is not, so the canonical
/// order is what makes a replayed centroid byte-identical.
#[must_use]
pub fn behavior_centroid(sample: &[&[f32]]) -> Option<Vec<f32>> {
    let first = sample.first()?;
    let dimension = first.len();
    if dimension == 0 || sample.iter().any(|v| v.len() != dimension) {
        return None;
    }
    let mut mean = vec![0.0_f64; dimension];
    for vector in sample {
        let norm: f64 = vector.iter().map(|x| f64::from(*x) * f64::from(*x)).sum();
        if norm == 0.0 || !norm.is_finite() {
            // A degenerate vector drops out of the sample rather than skewing it.
            continue;
        }
        let norm = norm.sqrt();
        for (slot, x) in mean.iter_mut().zip(vector.iter()) {
            *slot += f64::from(*x) / norm;
        }
    }
    let mean_norm: f64 = mean.iter().map(|x| x * x).sum();
    if mean_norm == 0.0 || !mean_norm.is_finite() {
        return None;
    }
    let mean_norm = mean_norm.sqrt();
    #[allow(clippy::cast_possible_truncation)]
    Some(mean.iter().map(|x| (x / mean_norm) as f32).collect())
}

/// The per-block drift score (05 §1): how much farther current behavior sits from
/// the attested identity anchor than baseline behavior did, in `[0, 1]`.
///
/// `clamp01(cosine(baseline_behavior, block) - cosine(current_behavior, block))` — a
/// positive score means behavior has *moved away* from the block since the baseline
/// was attested; movement toward the block (or no movement) scores `0.0` and can
/// never cross a threshold. Every degenerate input degrades through [`cosine`]'s
/// zero answer and the clamp, so the score is always a finite value in range: a
/// score that crosses a threshold is always one the arithmetic can vouch for.
#[must_use]
pub fn drift_score(
    baseline_behavior: &[f32],
    block_embedding: &[f32],
    current_behavior: &[f32],
) -> f64 {
    let anchored_then = cosine(baseline_behavior, block_embedding);
    let anchored_now = cosine(current_behavior, block_embedding);
    (anchored_then - anchored_now).clamp(0.0, 1.0)
}

/// Whether a drift score crosses the warning threshold. Non-finite scores never
/// cross (the detector warns only on values it can vouch for), and the comparison is
/// `>=` so a threshold of exactly the score fires — the same boundary convention as
/// [`is_eligible`](crate::decay::is_eligible).
#[must_use]
pub fn crosses_threshold(score: f64, threshold: f64) -> bool {
    score.is_finite() && threshold.is_finite() && score >= threshold
}

/// Whether a new fact sits close enough to a core block to be cooled (05 §1): the
/// conservative proximity trigger. A core block has no subject-predicate-object key,
/// so true contradiction is not deterministically decidable over free text — instead
/// *every* fact proximate to a high-trust core block cools, the safe
/// over-approximation (an affirming fact loses a little rank for a window; a
/// contradicting one is held back from influence exactly as 05 §1 asks). A
/// non-finite threshold never cools — the trigger acts only on configuration the
/// arithmetic can vouch for.
#[must_use]
pub fn is_core_proximate(
    fact_embedding: &[f32],
    block_embedding: &[f32],
    proximity_threshold: f64,
) -> bool {
    proximity_threshold.is_finite()
        && cosine(fact_embedding, block_embedding) >= proximity_threshold
}

/// The rank-time trust of a possibly-cooled fact (05 §1): the stored trust scalar,
/// reduced by `cooling_factor` while `now` sits inside the cooling window, untouched
/// once the window passes — a pure read-time modulation that is **never written
/// back**, so it survives a reliability refold (which recomputes the stored scalar)
/// and expires without a write (the comparison simply stops applying).
///
/// Conservative guards, sparing-only: a non-finite stored trust passes through
/// unmodulated (garbage in, same garbage out — never a minted `NaN`), and a
/// non-finite or out-of-`(0, 1]` factor is inert (a misconfigured factor must not
/// zero a fact's rank). An absent stamp is "never cooled".
#[must_use]
pub fn effective_cooled_trust(
    stored_trust: f64,
    cooled_until: Option<&Timestamp>,
    now: &Timestamp,
    cooling_factor: f64,
) -> f64 {
    let Some(cooled_until) = cooled_until else {
        return stored_trust;
    };
    if !stored_trust.is_finite()
        || !cooling_factor.is_finite()
        || cooling_factor <= 0.0
        || cooling_factor > 1.0
    {
        return stored_trust;
    }
    if now.timestamp() < cooled_until.timestamp() {
        stored_trust * cooling_factor
    } else {
        stored_trust
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-9;

    fn at(hour: u32) -> Timestamp {
        format!("2026-06-10T{hour:02}:00:00-05:00[America/Chicago]")
            .parse()
            .expect("valid zoned datetime")
    }

    #[test]
    fn cosine_matches_the_house_conventions() {
        assert!((cosine(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]) - 1.0).abs() < EPS);
        assert!((cosine(&[1.0, 0.0], &[0.0, 1.0])).abs() < EPS, "orthogonal");
        // Opposed vectors clamp to the floor rather than going negative.
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0])).abs() < EPS);
        // Mismatched lengths and zero norms answer "no measurable similarity".
        assert!((cosine(&[1.0], &[1.0, 0.0])).abs() < EPS);
        assert!((cosine(&[0.0, 0.0], &[1.0, 0.0])).abs() < EPS);
        // A non-finite component cannot escape as NaN.
        assert!(cosine(&[f32::NAN, 1.0], &[1.0, 1.0]).is_finite());
    }

    #[test]
    fn the_centroid_is_a_unit_vector_with_degenerates_dropped() {
        let a: &[f32] = &[2.0, 0.0];
        let b: &[f32] = &[0.0, 4.0];
        let zero: &[f32] = &[0.0, 0.0];
        let centroid = behavior_centroid(&[a, b, zero]).expect("two usable vectors");
        // Normalization first: the magnitudes 2 and 4 contribute equally, so the
        // centroid bisects the axes; the zero vector dropped out.
        assert!((f64::from(centroid[0]) - f64::from(centroid[1])).abs() < 1e-6);
        let norm: f64 = centroid.iter().map(|x| f64::from(*x) * f64::from(*x)).sum();
        assert!((norm - 1.0).abs() < 1e-6, "re-normalized to unit length");
    }

    #[test]
    fn unmeasurable_samples_are_the_skip_condition() {
        assert!(behavior_centroid(&[]).is_none(), "empty sample");
        let a: &[f32] = &[1.0, 0.0];
        let short: &[f32] = &[1.0];
        assert!(
            behavior_centroid(&[a, short]).is_none(),
            "mismatched dimensions"
        );
        let up: &[f32] = &[1.0, 0.0];
        let down: &[f32] = &[-1.0, 0.0];
        assert!(
            behavior_centroid(&[up, down]).is_none(),
            "behavior pointing every which way cancels to nothing measurable"
        );
    }

    #[test]
    fn drift_scores_movement_away_from_the_anchor() {
        let block = [1.0_f32, 0.0];
        let aligned = [1.0_f32, 0.1];
        let drifted = [0.2_f32, 1.0];
        let score = drift_score(&aligned, &block, &drifted);
        assert!(score > 0.5, "a large move away scores high: {score}");
        // Movement toward the anchor never reads as drift.
        let toward = drift_score(&drifted, &block, &aligned);
        assert!((toward).abs() < EPS, "improvement clamps to zero: {toward}");
        // Identical behavior is zero drift.
        assert!((drift_score(&aligned, &block, &aligned)).abs() < EPS);
        // Degenerates degrade to a vouched-for in-range value, never NaN.
        let degenerate = drift_score(&[], &block, &drifted);
        assert!(degenerate.is_finite() && (0.0..=1.0).contains(&degenerate));
    }

    #[test]
    fn threshold_crossing_is_at_or_above_and_never_on_garbage() {
        assert!(crosses_threshold(0.15, 0.15), "at the threshold fires");
        assert!(!crosses_threshold(0.149, 0.15));
        assert!(!crosses_threshold(f64::NAN, 0.15), "NaN never crosses");
        assert!(
            !crosses_threshold(0.5, f64::NAN),
            "garbage config never fires"
        );
    }

    #[test]
    fn proximity_cools_conservatively_and_only_on_vouched_config() {
        let block = [1.0_f32, 0.0];
        assert!(is_core_proximate(&[0.95, 0.05], &block, 0.75));
        assert!(!is_core_proximate(&[0.0, 1.0], &block, 0.75));
        assert!(
            !is_core_proximate(&[1.0, 0.0], &block, f64::NAN),
            "a non-finite threshold never cools"
        );
    }

    #[test]
    fn cooling_reduces_inside_the_window_and_expires_without_a_write() {
        let stamp = at(12);
        let inside = effective_cooled_trust(0.8, Some(&stamp), &at(6), 0.5);
        assert!((inside - 0.4).abs() < EPS, "inside the window: {inside}");
        let at_expiry = effective_cooled_trust(0.8, Some(&stamp), &at(12), 0.5);
        assert!(
            (at_expiry - 0.8).abs() < EPS,
            "the stamp instant itself is expired (strictly-before window)"
        );
        let after = effective_cooled_trust(0.8, Some(&stamp), &at(18), 0.5);
        assert!((after - 0.8).abs() < EPS, "expiry needs no write");
        let never = effective_cooled_trust(0.8, None, &at(6), 0.5);
        assert!((never - 0.8).abs() < EPS, "absent stamp is never cooled");
    }

    #[test]
    fn cooling_guards_spare_rather_than_doom() {
        let stamp = at(12);
        // Garbage stored trust passes through unmodulated, never a minted NaN.
        assert!(effective_cooled_trust(f64::NAN, Some(&stamp), &at(6), 0.5).is_nan());
        // A misconfigured factor is inert: zero would erase the fact's rank, above
        // one would *boost* a cooled fact, negative is nonsense — all spare.
        for factor in [0.0, -0.5, 1.5, f64::NAN, f64::INFINITY] {
            let trust = effective_cooled_trust(0.8, Some(&stamp), &at(6), factor);
            assert!(
                (trust - 0.8).abs() < EPS,
                "factor {factor} must be inert, got {trust}"
            );
        }
        // The factor of exactly 1.0 is a valid no-op posture.
        let unity = effective_cooled_trust(0.8, Some(&stamp), &at(6), 1.0);
        assert!((unity - 0.8).abs() < EPS);
    }
}
