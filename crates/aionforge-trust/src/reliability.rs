//! The pure reliability fold and its policy (06 §5, M4.T05).
//!
//! Trust is **doubly-derived state**. The canonical record is an append-only multiset of
//! `ReliabilityUpdate` audit events; `Agent.trust_scores` is a recomputable cache the fold
//! rewrites. This module is the cache's pure core: it replays one agent's decoded events into
//! a per-category Beta `(alpha, beta, score)` over the policy's prior, with no graph I/O. The
//! orchestrator (the `ReliabilityScorer`, a later PR) does the store reads that decode the
//! events and the writes that persist the result; the math lives here so the orchestrator and
//! its tests share one implementation, the same way [`aionforge_domain::trust::beta_posterior`]
//! backs the promotion posterior.
//!
//! Two properties make the fold safe to run off-cursor and replay after a crash:
//!
//! - **Order-independent.** Each event is summed in canonical id order, so the floating-point
//!   result is byte-identical no matter what order the events arrived in.
//! - **Idempotent.** Events are keyed by their content-addressed id, so a re-decoded duplicate
//!   (a double trigger, a crash-replay) collapses to one increment rather than counting twice.
//!
//! Both fall out of one structure: a [`BTreeMap`] keyed by the event id, which iterates in id
//! order *and* dedups by id. The fold never consults the policy's weights — each event already
//! carries the fixed weight it was emitted with, so a later retune of the weights moves only
//! *new* events and the replay of old ones is unchanged. That is what keeps the cache a pure
//! function of the canonical log (no read-modify-write on the score it is recomputing).

use std::collections::BTreeMap;

use aionforge_domain::ids::Id;
use aionforge_domain::nodes::agent::{TrustCategory, TrustScores};

/// Whether a reliability event raises or lowers an agent's score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReliabilityOutcome {
    /// An agreement: a later, distinct-authored canonical fact corroborated the agent's
    /// assertion. Adds the event weight to the category's Beta `alpha` (toward reliable).
    Success,
    /// A decay: a produced fact was contradicted and quarantined, or an attested fact was
    /// later invalidated. Adds the event weight to the category's Beta `beta` (toward
    /// unreliable).
    Failure,
}

/// One decoded reliability update — a single Beta pseudo-count increment against one agent.
///
/// The canonical record is an immutable `ReliabilityUpdate` audit event; this is its decoded,
/// fold-ready form. [`event_id`](ReliabilityEvent::event_id) is that event's content-addressed
/// id, which the fold uses as both the canonical summation order (byte-identical replay) and
/// the idempotency key (a re-decoded duplicate collapses to one).
#[derive(Debug, Clone, PartialEq)]
pub struct ReliabilityEvent {
    /// The content-addressed id of the source `ReliabilityUpdate` audit event.
    pub event_id: Id,
    /// The trust category this update lands in (the predicate bucket or attestation category).
    pub category: String,
    /// Whether the update raises (`Success`) or lowers (`Failure`) the score.
    pub outcome: ReliabilityOutcome,
    /// The Beta pseudo-count this event contributes — the policy weight fixed at emission
    /// (`w_agree` for a `Success`, `w_contradict` / `w_attest_invalid` for a `Failure`).
    pub weight: f64,
}

/// The trust-scoring policy the fold and (later) the scorer apply (06 §5). Mirrors
/// `aionforge-config`'s `ReliabilityConfig`; the host maps one to the other, the same way
/// `PromotionPolicy` mirrors `PromotionConfig`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReliabilityPolicy {
    /// Whether trust scoring runs at all. Off ⇒ no event is ever emitted and the fold is
    /// never invoked. The fold itself does not consult this flag (it is a pure replay); the
    /// orchestrator gates on it.
    pub enabled: bool,
    /// The Beta prior `alpha` (pseudo-count of reliable outcomes) every category folds from.
    pub prior_alpha: f64,
    /// The Beta prior `beta` (pseudo-count of unreliable outcomes).
    pub prior_beta: f64,
    /// The category an uncategorized update falls into.
    pub default_category: String,
    /// The decay weight for a producing agent whose fact was contradicted and quarantined.
    pub w_contradict: f64,
    /// The decay weight for an attesting agent whose attested fact was later invalidated.
    pub w_attest_invalid: f64,
    /// The agreement gain a producing agent earns from a later distinct-authored corroboration.
    /// Strictly below `w_contradict` so reliability cannot be farmed back to neutral.
    pub w_agree: f64,
}

impl Default for ReliabilityPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            prior_alpha: 1.0,
            prior_beta: 1.0,
            default_category: "reliability".to_string(),
            w_contradict: 1.0,
            w_attest_invalid: 1.0,
            w_agree: 0.25,
        }
    }
}

impl ReliabilityPolicy {
    /// Validate the policy when it is on (06 §5): finite positive priors, a non-empty default
    /// category, finite non-negative weights, and the asymmetry guard `w_agree < w_contradict`.
    ///
    /// The asymmetry guard is the farming bound: a producer's agreement gain must stay strictly
    /// below its contradiction decay, so a corroboration can never fully offset a contradiction.
    /// The attester channel is loss-only, so it carries no analogous guard. An off policy is
    /// always valid — its weights are inert.
    ///
    /// # Errors
    /// Returns a message naming the offending field when a bound is violated.
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if !self.prior_alpha.is_finite() || self.prior_alpha <= 0.0 {
            return Err(
                "reliability.prior_alpha must be a finite value greater than zero".to_string(),
            );
        }
        if !self.prior_beta.is_finite() || self.prior_beta <= 0.0 {
            return Err(
                "reliability.prior_beta must be a finite value greater than zero".to_string(),
            );
        }
        if self.default_category.trim().is_empty() {
            return Err("reliability.default_category must not be empty".to_string());
        }
        for (key, weight) in [
            ("reliability.w_contradict", self.w_contradict),
            ("reliability.w_attest_invalid", self.w_attest_invalid),
            ("reliability.w_agree", self.w_agree),
        ] {
            if !weight.is_finite() || weight < 0.0 {
                return Err(format!(
                    "{key} must be a finite value greater than or equal to zero"
                ));
            }
        }
        // Both weights are already known finite from the loop above, so a plain `>=` is
        // well-defined (no NaN can reach here).
        if self.w_agree >= self.w_contradict {
            return Err(
                "reliability.w_agree must be strictly less than reliability.w_contradict \
                        (agreement gain must not outpace contradiction decay)"
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// The pure reliability fold: one agent's decoded events → its per-category [`TrustScores`].
///
/// A zero-sized namespace for the fold; it owns no state, so the result is a pure function of
/// `(policy.prior_*, events)`.
pub struct ReliabilityFold;

impl ReliabilityFold {
    /// Replay an agent's reliability events into a per-category Beta `(alpha, beta, score)`.
    ///
    /// Each category folds independently from the prior: a `Success` adds its weight to
    /// `alpha`, a `Failure` to `beta`, and `score = alpha / (alpha + beta)` is the posterior
    /// mean. Only categories that carry at least one event appear in the result; a category an
    /// agent has no events in is absent, and the consumer reads its absence as the neutral
    /// prior rather than the fold inventing an entry.
    ///
    /// The fold is order-independent and idempotent by construction (see the module docs): a
    /// [`BTreeMap`] keyed by the event id dedups duplicates and fixes the summation order, so a
    /// shuffled or replayed event multiset yields a byte-identical result. A corrupt weight
    /// (non-finite or negative) is sanitized to a no-op increment, so given positive priors and
    /// finite, non-negative weights the score stays inside `(0, 1)`. (A pathologically huge
    /// weight — far outside any policy-emitted value, since policy weights are `O(1)` and
    /// config-validated — can saturate the f64 score to `0.0` or `1.0`; `sanitize_weight` only
    /// neutralizes the non-finite and negative corruption that can actually reach the fold.)
    #[must_use]
    pub fn fold(policy: &ReliabilityPolicy, events: &[ReliabilityEvent]) -> TrustScores {
        // Dedup by content-addressed id and fix the order in one structure: the BTreeMap
        // collapses a duplicate id (idempotent) and iterates in id order (deterministic sum).
        let mut unique: BTreeMap<Id, &ReliabilityEvent> = BTreeMap::new();
        for event in events {
            unique.insert(event.event_id, event);
        }

        // Bucket per category, accumulating from the prior in the id order the BTreeMap yields.
        let mut categories: BTreeMap<String, (f64, f64)> = BTreeMap::new();
        for event in unique.values() {
            let weight = sanitize_weight(event.weight);
            let entry = categories
                .entry(event.category.clone())
                .or_insert((policy.prior_alpha, policy.prior_beta));
            match event.outcome {
                ReliabilityOutcome::Success => entry.0 += weight,
                ReliabilityOutcome::Failure => entry.1 += weight,
            }
        }

        let scores = categories
            .into_iter()
            .map(|(category, (alpha, beta))| {
                let score = alpha / (alpha + beta);
                (category, TrustCategory { alpha, beta, score })
            })
            .collect();
        TrustScores(scores)
    }
}

/// Sanitize a fold weight so a corrupt event can never push a pseudo-count negative or
/// non-finite: a non-finite weight contributes nothing and a negative one is clamped to zero.
/// A well-formed event always carries a finite, non-negative policy weight, so this only bites
/// on corruption — and when it does, the score stays bounded rather than poisoned.
fn sanitize_weight(weight: f64) -> f64 {
    if weight.is_finite() {
        weight.max(0.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use uuid::Uuid;

    const EPS: f64 = 1e-12;

    fn id(seed: u128) -> Id {
        Id::from_uuid(Uuid::from_u128(seed))
    }

    fn event(
        seed: u128,
        category: &str,
        outcome: ReliabilityOutcome,
        weight: f64,
    ) -> ReliabilityEvent {
        ReliabilityEvent {
            event_id: id(seed),
            category: category.to_string(),
            outcome,
            weight,
        }
    }

    fn uniform_policy() -> ReliabilityPolicy {
        ReliabilityPolicy {
            enabled: true,
            ..ReliabilityPolicy::default()
        }
    }

    fn score_of(scores: &TrustScores, category: &str) -> f64 {
        scores.0.get(category).expect("category present").score
    }

    // --- the fold's value semantics ---------------------------------------------------------

    #[test]
    fn no_events_yield_no_categories() {
        let scores = ReliabilityFold::fold(&uniform_policy(), &[]);
        assert!(
            scores.0.is_empty(),
            "an agent with no events folds to an empty map"
        );
    }

    #[test]
    fn the_uniform_prior_mean_is_one_half() {
        // A single zero-weight event materializes the category at the bare prior.
        let events = [event(1, "reliability", ReliabilityOutcome::Success, 0.0)];
        let scores = ReliabilityFold::fold(&uniform_policy(), &events);
        assert!((score_of(&scores, "reliability") - 0.5).abs() < EPS);
    }

    #[test]
    fn one_contradiction_decays_to_one_third() {
        // alpha=1, beta=1+1=2, score=1/3.
        let events = [event(1, "reliability", ReliabilityOutcome::Failure, 1.0)];
        let scores = ReliabilityFold::fold(&uniform_policy(), &events);
        assert!((score_of(&scores, "reliability") - 1.0 / 3.0).abs() < EPS);
    }

    #[test]
    fn one_agreement_raises_the_score_only_a_little() {
        // alpha=1+0.25=1.25, beta=1, score=1.25/2.25 ~ 0.5556.
        let events = [event(1, "reliability", ReliabilityOutcome::Success, 0.25)];
        let scores = ReliabilityFold::fold(&uniform_policy(), &events);
        assert!((score_of(&scores, "reliability") - 1.25 / 2.25).abs() < EPS);
    }

    #[test]
    fn categories_fold_independently() {
        let events = [
            event(1, "code", ReliabilityOutcome::Failure, 1.0),
            event(2, "code", ReliabilityOutcome::Failure, 1.0),
            event(3, "pii", ReliabilityOutcome::Success, 0.25),
        ];
        let scores = ReliabilityFold::fold(&uniform_policy(), &events);
        // code: alpha=1, beta=1+2=3 → 0.25. pii: alpha=1.25, beta=1 → 0.5556.
        assert!((score_of(&scores, "code") - 0.25).abs() < EPS);
        assert!((score_of(&scores, "pii") - 1.25 / 2.25).abs() < EPS);
    }

    #[test]
    fn a_duplicate_event_id_is_counted_once() {
        let once = [event(7, "reliability", ReliabilityOutcome::Failure, 1.0)];
        let twice = [
            event(7, "reliability", ReliabilityOutcome::Failure, 1.0),
            event(7, "reliability", ReliabilityOutcome::Failure, 1.0),
        ];
        assert_eq!(
            ReliabilityFold::fold(&uniform_policy(), &once),
            ReliabilityFold::fold(&uniform_policy(), &twice),
            "a replayed event id folds to one increment"
        );
    }

    #[test]
    fn the_prior_seeds_each_category_in_the_correct_slot() {
        // An asymmetric Beta(2, 5) prior catches a slot swap that the symmetric (1, 1) default
        // would hide. Assert alpha and beta directly, not just the score.
        let policy = ReliabilityPolicy {
            enabled: true,
            prior_alpha: 2.0,
            prior_beta: 5.0,
            ..ReliabilityPolicy::default()
        };
        // A zero-weight event materializes the category at the bare prior.
        let bare = ReliabilityFold::fold(
            &policy,
            &[event(1, "reliability", ReliabilityOutcome::Success, 0.0)],
        );
        let c = bare.0.get("reliability").expect("category present");
        assert!((c.alpha - 2.0).abs() < EPS, "prior_alpha lands in alpha");
        assert!((c.beta - 5.0).abs() < EPS, "prior_beta lands in beta");
        assert!((c.score - 2.0 / 7.0).abs() < EPS);
        // A unit Failure increments the prior-seeded beta, leaving alpha at the prior.
        let decayed = ReliabilityFold::fold(
            &policy,
            &[event(1, "reliability", ReliabilityOutcome::Failure, 1.0)],
        );
        let c = decayed.0.get("reliability").expect("category present");
        assert!((c.alpha - 2.0).abs() < EPS);
        assert!((c.beta - 6.0).abs() < EPS);
        assert!((c.score - 2.0 / 8.0).abs() < EPS);
    }

    #[test]
    fn one_contradiction_outweighs_one_agreement() {
        // From the bare prior the magnitudes are asymmetric: |0.5 - 1/3| > |0.5 - 1.25/2.25|.
        // A constant inequality, so it belongs in a plain test, not under proptest.
        let policy = uniform_policy();
        let down = ReliabilityFold::fold(
            &policy,
            &[event(
                1,
                "reliability",
                ReliabilityOutcome::Failure,
                policy.w_contradict,
            )],
        );
        let up = ReliabilityFold::fold(
            &policy,
            &[event(
                1,
                "reliability",
                ReliabilityOutcome::Success,
                policy.w_agree,
            )],
        );
        let drop = 0.5 - score_of(&down, "reliability");
        let gain = score_of(&up, "reliability") - 0.5;
        assert!(drop > gain, "one contradiction outweighs one agreement");
    }

    // --- policy validation ------------------------------------------------------------------

    #[test]
    fn an_off_policy_ignores_its_weights() {
        let policy = ReliabilityPolicy {
            enabled: false,
            w_agree: 5.0,
            w_contradict: 1.0,
            prior_alpha: -1.0,
            ..ReliabilityPolicy::default()
        };
        policy
            .validate()
            .expect("an off policy is inert and always valid");
    }

    #[test]
    fn agreement_gain_at_or_above_contradiction_decay_is_rejected() {
        let mut policy = uniform_policy();
        policy.w_agree = 1.0; // equal to w_contradict
        assert!(
            policy
                .validate()
                .is_err_and(|m| m.contains("reliability.w_agree")),
            "equal weights are rejected (no strict asymmetry)"
        );
        policy.w_agree = 2.0; // above w_contradict
        assert!(
            policy
                .validate()
                .is_err_and(|m| m.contains("reliability.w_agree"))
        );
    }

    #[test]
    fn a_decay_only_policy_is_valid() {
        let mut policy = uniform_policy();
        policy.w_agree = 0.0;
        policy
            .validate()
            .expect("w_agree = 0 is a valid decay-only posture");
    }

    #[test]
    fn non_finite_and_negative_priors_and_weights_are_rejected() {
        for (mutate, needle) in [
            (
                Box::new(|p: &mut ReliabilityPolicy| p.prior_alpha = 0.0)
                    as Box<dyn Fn(&mut ReliabilityPolicy)>,
                "prior_alpha",
            ),
            (
                Box::new(|p: &mut ReliabilityPolicy| p.prior_beta = f64::NAN),
                "prior_beta",
            ),
            (
                Box::new(|p: &mut ReliabilityPolicy| p.w_contradict = -1.0),
                "w_contradict",
            ),
            (
                Box::new(|p: &mut ReliabilityPolicy| p.w_attest_invalid = f64::INFINITY),
                "w_attest_invalid",
            ),
        ] {
            let mut policy = uniform_policy();
            mutate(&mut policy);
            assert!(
                policy.validate().is_err_and(|m| m.contains(needle)),
                "a bad {needle} is rejected"
            );
        }
    }

    #[test]
    fn an_empty_default_category_is_rejected() {
        let mut policy = uniform_policy();
        policy.default_category = "  ".to_string();
        assert!(
            policy
                .validate()
                .is_err_and(|m| m.contains("default_category"))
        );
    }

    // --- the four ratified properties -------------------------------------------------------

    // A strategy for a small multiset of well-formed events over a few categories.
    fn events_strategy() -> impl Strategy<Value = Vec<ReliabilityEvent>> {
        let one = (
            any::<u64>(),
            prop_oneof!["reliability", "code", "pii"],
            any::<bool>(),
            0.0f64..4.0,
        )
            .prop_map(|(seed, category, success, weight)| {
                let outcome = if success {
                    ReliabilityOutcome::Success
                } else {
                    ReliabilityOutcome::Failure
                };
                event(u128::from(seed), &category, outcome, weight)
            });
        prop::collection::vec(one, 0..32)
    }

    proptest! {
        /// Commutativity: the fold depends only on the event *multiset*, not its order — so a
        /// shuffle yields a byte-identical result.
        #[test]
        fn fold_is_order_independent(mut events in events_strategy(), seed in any::<u64>()) {
            let forward = ReliabilityFold::fold(&uniform_policy(), &events);
            // A cheap deterministic permutation: rotate by a seed-derived amount, then reverse.
            if !events.is_empty() {
                let rot = (seed as usize) % events.len();
                events.rotate_left(rot);
            }
            events.reverse();
            let shuffled = ReliabilityFold::fold(&uniform_policy(), &events);
            prop_assert_eq!(forward, shuffled);
        }

        /// Idempotency: folding a multiset and folding it with every event duplicated give the
        /// same result, because the id-keyed dedup collapses the copies.
        #[test]
        fn fold_is_idempotent_under_replay(events in events_strategy()) {
            let once = ReliabilityFold::fold(&uniform_policy(), &events);
            let mut replayed = events.clone();
            replayed.extend(events.iter().cloned());
            let twice = ReliabilityFold::fold(&uniform_policy(), &replayed);
            prop_assert_eq!(once, twice);
        }

        /// Bounded score: for finite, non-negative weights — and any non-finite or negative
        /// weight, which is sanitized to a no-op — every category score is finite and strictly
        /// inside `(0, 1)`, because positive priors plus sanitized weights keep both
        /// pseudo-counts at or above the prior. Categories are drawn from a small set, so the
        /// bound is checked across several simultaneous buckets.
        #[test]
        fn every_score_stays_in_the_open_unit_interval(
            raw in prop::collection::vec(
                (any::<u64>(), prop_oneof!["reliability", "code", "pii"], any::<bool>(), prop_oneof![Just(0.0), Just(-1.0), Just(f64::NAN), Just(f64::INFINITY), 0.0f64..1e9]),
                0..32,
            ),
        ) {
            let events: Vec<_> = raw
                .into_iter()
                .map(|(seed, category, success, weight)| {
                    let outcome = if success { ReliabilityOutcome::Success } else { ReliabilityOutcome::Failure };
                    event(u128::from(seed), &category, outcome, weight)
                })
                .collect();
            let scores = ReliabilityFold::fold(&uniform_policy(), &events);
            for category in scores.0.values() {
                prop_assert!(category.score.is_finite());
                prop_assert!(category.score > 0.0 && category.score < 1.0);
                prop_assert!(category.alpha >= 1.0 && category.beta >= 1.0, "weights never go below the prior");
            }
        }

        /// Monotonicity: adding a `Failure` never raises a category's score and a `Success`
        /// never lowers it, for any base multiset. (The asymmetry *magnitude* — a contradiction
        /// outweighs an agreement — is a constant inequality, proved in the plain test
        /// `one_contradiction_outweighs_one_agreement`.)
        #[test]
        fn outcomes_are_monotone_in_each_direction(base in events_strategy(), seed in any::<u64>()) {
            let policy = uniform_policy();
            let before = ReliabilityFold::fold(&policy, &base);
            let before_score = before.0.get("reliability").map_or(0.5, |c| c.score);

            // A fresh id (above the u64 seed range the base draws from) so it adds, not dedups.
            let fresh = u128::from(seed) + (1u128 << 96);

            let mut with_failure = base.clone();
            with_failure.push(event(fresh, "reliability", ReliabilityOutcome::Failure, policy.w_contradict));
            let after_failure = ReliabilityFold::fold(&policy, &with_failure);
            prop_assert!(score_of(&after_failure, "reliability") <= before_score + EPS, "a failure never raises the score");

            let mut with_success = base.clone();
            with_success.push(event(fresh, "reliability", ReliabilityOutcome::Success, policy.w_agree));
            let after_success = ReliabilityFold::fold(&policy, &with_success);
            prop_assert!(score_of(&after_success, "reliability") >= before_score - EPS, "a success never lowers the score");
        }
    }
}
