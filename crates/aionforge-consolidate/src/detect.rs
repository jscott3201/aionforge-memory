//! Supersession / contradiction detection (write-and-consolidation §2, M2.T05b).
//!
//! A pure function over the committed current facts and the episode's newly extracted
//! facts: it decides, conservatively and deterministically, which new facts supersede a
//! prior one (functional predicate, newer different object) and which contradict one
//! (mutually-exclusive object), and whether a contradiction should quarantine the new
//! fact (a high-trust incumbent). It produces only INSTRUCTIONS — the store materializes
//! them in the flip transaction. Being store-free, every branch is unit-testable.
//!
//! **Convergence (06 §2).** A functional `(subject, predicate)` holds exactly one current
//! object, and which object wins is the **K1 order**: the assertion with the greater
//! event time (`valid_from`) wins, and a simultaneous tie — equal `valid_from`, which for a
//! functional predicate always means two distinct objects — is settled by
//! [`object_order_key`], the canonical object order. Both components are a pure function of
//! the assertion itself (never the substrate's arrival clock, and deliberately **not** the
//! content-hash `Fact.id` or the originating agent, both of which are fixed by whichever
//! episode wins the dedup race and so are arrival-fragile), so the winner is identical under
//! any consolidation order. The comparison is symmetric: the loser is superseded by the
//! winner whichever side it is on, so a stale assertion arriving after a newer incumbent is
//! retired into history rather than lingering as a second current value.

use std::collections::BTreeMap;

use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::forensic::AuditEvent;
use aionforge_domain::nodes::semantic::Fact;
use aionforge_domain::time::Timestamp;
use aionforge_domain::value::ObjectValue;
use aionforge_store::{Contradiction, FactKey, MaterializedFact, Supersession};

use crate::config::DetectionConfig;
use crate::merge::{new_is_contradiction_victim, new_wins_functional_slot, object_order_key};

/// A committed current fact (no live supersession/contradiction), projected for detection.
pub(crate) struct CurrentFact {
    /// The fact's node id — needed to name it as the quarantine victim when a contradiction
    /// resolves against it (the victim can be the incumbent, not only the new fact).
    pub id: Id,
    /// The fact's identifying triple.
    pub key: FactKey,
    /// The fact's event-time `valid_from` (its `ABOUT` window open instant).
    pub valid_from: Timestamp,
    /// The fact's derivation/writer trust (drives the quarantine decision).
    pub trust: f64,
}

/// The instructions detection produced for one episode.
#[derive(Default)]
pub(crate) struct DetectionOutput {
    /// A newer fact supersedes a prior current one (functional predicate).
    pub supersessions: Vec<Supersession>,
    /// A new fact contradicts a current one (with optional quarantine of the new fact).
    pub contradictions: Vec<Contradiction>,
    /// Quarantine reconcile-signal audit events.
    pub audits: Vec<AuditEvent>,
}

/// Detect supersession/contradiction of `new_facts` against the committed `current` set.
///
/// `captured_at` is the new facts' event time (the episode's), `now` the transaction
/// time, `actor_id` the consolidator's audit actor. Pure: no store access.
pub(crate) fn detect(
    current: &[CurrentFact],
    new_facts: &[MaterializedFact],
    cfg: &DetectionConfig,
    namespace: &Namespace,
    captured_at: &Timestamp,
    now: &Timestamp,
    actor_id: &Id,
) -> DetectionOutput {
    let mut out = DetectionOutput::default();
    if !cfg.enabled {
        return out;
    }

    // For each functional (subject, predicate) the episode touches, pick the one new fact
    // that wins — the object that sorts first under `object_order_key` (see
    // `detect_intra_episode_ties`, which uses the same rule to retire the losers). An
    // incumbent is then superseded only by that winner, never by a losing peer. This matters
    // when an episode asserts two new values for one functional predicate (e.g. both SF and
    // Boston for `based_in`) against an NYC incumbent: routing every retirement to the single
    // survivor means NYC and the loser each get exactly one `SUPERSEDED_BY` edge to the
    // winner, so correctness does not rest on the store absorbing redundant,
    // differently-targeted supersessions of one incumbent.
    let survivors = functional_survivors(new_facts, cfg);

    // New facts vs the committed current set, scoped to the same (subject, predicate).
    for materialized in new_facts {
        let new_key = fact_key(&materialized.fact);
        let rule = cfg.rule(&new_key.predicate);
        let is_survivor = survivors
            .get(&(new_key.subject_id.to_string(), new_key.predicate.clone()))
            .is_none_or(|winner| *winner == materialized.fact.identity.id.to_string());
        for incumbent in current.iter().filter(|c| {
            c.key.subject_id == new_key.subject_id && c.key.predicate == new_key.predicate
        }) {
            if incumbent.key.object == new_key.object {
                continue; // the same triple — T04a dedup handles it, not a conflict
            }
            if rule.functional && is_survivor {
                // The single functional slot is settled by the K1 order (see the module
                // doc): the winner is a pure function of the two assertions, so the same
                // object ends up current under any consolidation order. The loser is
                // superseded by the winner — retained in history, never dropped.
                if new_wins_functional_slot(
                    &new_key.object,
                    captured_at,
                    &incumbent.key.object,
                    &incumbent.valid_from,
                ) {
                    // The new assertion wins (strictly later, or the tie-winning object):
                    // it retires the incumbent. Its window closes at the new event time,
                    // which is >= the incumbent's here, so the closed window stays ordered.
                    out.supersessions.push(Supersession {
                        old_fact: incumbent.key.clone(),
                        new_fact: new_key.clone(),
                        reason: "functional predicate superseded by a newer assertion".to_string(),
                        valid_from: captured_at.clone(),
                    });
                } else {
                    // The incumbent wins: a stale assertion (older event time) arriving
                    // after a newer incumbent, or the losing side of a simultaneous tie.
                    // The new fact is born superseded — closing it at the incumbent's
                    // `valid_from` keeps its window [new.valid_from, incumbent.valid_from)
                    // ordered (the new event time is <= the incumbent's on this branch).
                    // The old forward-only guard never produced this direction, which is
                    // what let a stale fact linger as a second current value — a divergence.
                    out.supersessions.push(Supersession {
                        old_fact: new_key.clone(),
                        new_fact: incumbent.key.clone(),
                        reason: "stale assertion superseded by a newer incumbent".to_string(),
                        valid_from: incumbent.valid_from.clone(),
                    });
                }
            } else if mutually_exclusive(&rule, &incumbent.key.object, &new_key.object) {
                // The contradiction's victim — the `CONTRADICTS` source, which the
                // `current_support_facts` provider excludes from recall by edge presence
                // (store providers.rs, `exclude_outgoing(CONTRADICTS)`), regardless of the
                // quarantine status — is the LOWER-TRUST side, ties settled by the smaller
                // object order. A pure function of the unordered pair {(trust, object)}, never
                // of which side is incumbent vs new, so the same contradiction excludes the
                // same value under any consolidation order (06 §2). The survivor stays current;
                // the victim is retained (node, edge, and — when quarantined — an audit signal).
                let new_trust = materialized.fact.stats.trust;
                let new_is_victim = new_is_contradiction_victim(
                    &new_key.object,
                    new_trust,
                    &incumbent.key.object,
                    incumbent.trust,
                );
                // Quarantine — actively flag the victim for review — only when the pair carries
                // real weight: either side at or above the high-trust bar. Symmetric in the
                // pair, not keyed on whichever side happened to be the incumbent.
                let quarantine = new_trust.max(incumbent.trust) >= cfg.high_trust_threshold;
                let (source_fact, target_fact) = if new_is_victim {
                    (new_key.clone(), incumbent.key.clone())
                } else {
                    (incumbent.key.clone(), new_key.clone())
                };
                out.contradictions.push(Contradiction {
                    source_fact,
                    target_fact,
                    detected_by: "detection-v1".to_string(),
                    quarantine_source: quarantine,
                    detected_at: captured_at.clone(),
                });
                if quarantine {
                    let (victim_id, victim_object, victim_trust) = if new_is_victim {
                        (materialized.fact.identity.id, &new_key.object, new_trust)
                    } else {
                        (incumbent.id, &incumbent.key.object, incumbent.trust)
                    };
                    let (survivor_object, survivor_trust) = if new_is_victim {
                        (&incumbent.key.object, incumbent.trust)
                    } else {
                        (&new_key.object, new_trust)
                    };
                    out.audits.push(crate::audit::quarantine_audit(
                        namespace,
                        &new_key.predicate,
                        &victim_id,
                        victim_object,
                        victim_trust,
                        survivor_object,
                        survivor_trust,
                        now,
                        actor_id,
                    ));
                }
            }
            // else independent — additive, no action.
        }
    }

    detect_intra_episode_ties(new_facts, cfg, captured_at, &mut out);
    out
}

/// The winning new fact id for each functional `(subject, predicate)` the episode asserts:
/// the one whose object sorts first under [`object_order_key`]. Every fact in an episode
/// shares the episode's `captured_at`, so the K1 order reduces here to the object order —
/// the same rule the cross-episode comparison uses, so intra- and cross-episode survivors
/// agree by construction. This is the single survivor every functional retirement — of an
/// incumbent or of a losing peer — points at, so the rule lives in exactly one place and
/// `detect` and `detect_intra_episode_ties` cannot disagree about who won.
fn functional_survivors(
    new_facts: &[MaterializedFact],
    cfg: &DetectionConfig,
) -> BTreeMap<(String, String), String> {
    // group -> (winning object order key, winning fact id)
    let mut survivors: BTreeMap<(String, String), (String, String)> = BTreeMap::new();
    for materialized in new_facts {
        let key = fact_key(&materialized.fact);
        if !cfg.rule(&key.predicate).functional {
            continue;
        }
        let group = (key.subject_id.to_string(), key.predicate);
        let object_key = object_order_key(&key.object);
        let id = materialized.fact.identity.id.to_string();
        survivors
            .entry(group)
            .and_modify(|(winning_object, winning_id)| {
                if object_key < *winning_object {
                    *winning_object = object_key.clone();
                    *winning_id = id.clone();
                }
            })
            .or_insert_with(|| (object_key.clone(), id.clone()));
    }
    survivors
        .into_iter()
        .map(|(group, (_object, id))| (group, id))
        .collect()
}

/// Among new facts that share a functional `(subject, predicate)`, keep the one whose
/// object sorts first under [`object_order_key`] (the `functional_survivors` winner) and
/// supersede the rest by it — a deterministic, clock-free tiebreak for the within-episode
/// case (every fact shares `captured_at`, so the K1 order reduces to the object order).
fn detect_intra_episode_ties(
    new_facts: &[MaterializedFact],
    cfg: &DetectionConfig,
    captured_at: &Timestamp,
    out: &mut DetectionOutput,
) {
    let mut groups: BTreeMap<(String, String), Vec<&MaterializedFact>> = BTreeMap::new();
    for materialized in new_facts {
        let key = fact_key(&materialized.fact);
        if cfg.rule(&key.predicate).functional {
            groups
                .entry((key.subject_id.to_string(), key.predicate))
                .or_default()
                .push(materialized);
        }
    }
    for (_, mut group) in groups {
        if group.len() < 2 {
            continue;
        }
        group.sort_by_key(|a| object_order_key(&a.fact.object));
        let survivor = fact_key(&group[0].fact);
        for loser in &group[1..] {
            let loser_key = fact_key(&loser.fact);
            if loser_key.object == survivor.object {
                continue; // identical object — dedup, not a tie
            }
            out.supersessions.push(Supersession {
                old_fact: loser_key,
                new_fact: survivor.clone(),
                reason: "intra_episode_functional_tie".to_string(),
                valid_from: captured_at.clone(),
            });
        }
    }
}

/// Whether two objects are mutually exclusive for a predicate: the always-on boolean
/// inversion rule, plus any configured antonym pairs (order-insensitive).
fn mutually_exclusive(
    rule: &crate::config::PredicateRule,
    a: &ObjectValue,
    b: &ObjectValue,
) -> bool {
    if let (ObjectValue::Bool(x), ObjectValue::Bool(y)) = (a, b)
        && x != y
    {
        return true;
    }
    rule.contradicts
        .iter()
        .any(|(p, q)| (p == a && q == b) || (p == b && q == a))
}

/// The identifying triple of a fact.
fn fact_key(fact: &Fact) -> FactKey {
    FactKey {
        subject_id: fact.subject_id,
        predicate: fact.predicate.clone(),
        object: fact.object.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionforge_domain::blocks::{Identity, Stats};
    use aionforge_domain::edges::About;
    use aionforge_domain::nodes::forensic::AuditKind;
    use aionforge_domain::nodes::semantic::FactStatus;
    use aionforge_domain::time::BiTemporal;

    use crate::config::PredicateRule;

    fn ts(text: &str) -> Timestamp {
        text.parse().expect("valid zoned datetime literal")
    }

    /// 09:00 — the incumbent's `valid_from` in most cases.
    fn t1() -> Timestamp {
        ts("2026-06-06T09:00:00Z[UTC]")
    }

    /// 11:00 — the new episode's `captured_at` (strictly after `t1`).
    fn t2() -> Timestamp {
        ts("2026-06-06T11:00:00Z[UTC]")
    }

    fn ns() -> Namespace {
        Namespace::Agent("tester".to_string())
    }

    fn stats(trust: f64) -> Stats {
        Stats {
            importance: 0.5,
            trust,
            last_access: t1(),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.0,
            is_pinned: false,
        }
    }

    /// A committed current fact opened at `t1`, with the given trust.
    fn current(subject: &Id, predicate: &str, object: ObjectValue, trust: f64) -> CurrentFact {
        current_at(subject, predicate, object, t1(), trust)
    }

    /// A committed current fact opened at an explicit `valid_from`, with the given trust.
    fn current_at(
        subject: &Id,
        predicate: &str,
        object: ObjectValue,
        valid_from: Timestamp,
        trust: f64,
    ) -> CurrentFact {
        CurrentFact {
            id: Id::generate(),
            key: FactKey {
                subject_id: *subject,
                predicate: predicate.to_string(),
                object,
            },
            valid_from,
            trust,
        }
    }

    /// A new materialized fact with an explicit id (so tie-break ordering is decidable).
    fn mfact_with_id(
        id: Id,
        subject: &Id,
        predicate: &str,
        object: ObjectValue,
    ) -> MaterializedFact {
        MaterializedFact {
            fact: Fact {
                identity: Identity {
                    id,
                    ingested_at: t2(),
                    namespace: ns(),
                    expired_at: None,
                },
                stats: stats(0.9),
                subject_id: *subject,
                predicate: predicate.to_string(),
                object,
                confidence: 0.9,
                status: FactStatus::Active,
                statement: String::new(),
                embedding: None,
                embedder_model: None,
                extraction: None,
                cooled_until: None,
            },
            about: About {
                temporal: BiTemporal {
                    valid_from: t2(),
                    valid_to: None,
                    ingested_at: t2(),
                    expired_at: None,
                },
            },
        }
    }

    fn mfact(subject: &Id, predicate: &str, object: ObjectValue) -> MaterializedFact {
        mfact_with_id(Id::generate(), subject, predicate, object)
    }

    /// A new materialized fact with an explicit writer trust (drives the contradiction
    /// quarantine decision).
    fn mfact_trust(
        subject: &Id,
        predicate: &str,
        object: ObjectValue,
        trust: f64,
    ) -> MaterializedFact {
        let mut materialized = mfact(subject, predicate, object);
        materialized.fact.stats.trust = trust;
        materialized
    }

    fn text(value: &str) -> ObjectValue {
        ObjectValue::Text(value.to_string())
    }

    /// Run detection with the standard event/transaction times and a throwaway actor.
    fn run(
        current: &[CurrentFact],
        new: &[MaterializedFact],
        cfg: &DetectionConfig,
    ) -> DetectionOutput {
        detect(current, new, cfg, &ns(), &t2(), &t2(), &Id::generate())
    }

    #[test]
    fn functional_predicate_supersedes_on_a_newer_object() {
        let cfg = DetectionConfig::with_default_rules(); // `based_in` is functional
        let subject = Id::generate();
        let cur = vec![current(&subject, "based_in", text("NYC"), 0.9)];
        let new = vec![mfact(&subject, "based_in", text("SF"))];

        let out = run(&cur, &new, &cfg);

        assert_eq!(out.supersessions.len(), 1, "one supersession");
        assert!(
            out.contradictions.is_empty(),
            "supersession, not contradiction"
        );
        let s = &out.supersessions[0];
        assert_eq!(
            s.old_fact.object,
            text("NYC"),
            "the prior object is retired"
        );
        assert_eq!(s.new_fact.object, text("SF"), "the newer object wins");
        assert_eq!(
            s.valid_from,
            t2(),
            "the window closes at the new event time"
        );
    }

    #[test]
    fn multi_valued_predicate_is_additive() {
        // `knows` is unregistered, so it is multi-valued and its objects are independent.
        let cfg = DetectionConfig::with_default_rules();
        let subject = Id::generate();
        let cur = vec![current(&subject, "knows", text("Rust"), 0.9)];
        let new = vec![mfact(&subject, "knows", text("Go"))];

        let out = run(&cur, &new, &cfg);

        assert!(out.supersessions.is_empty(), "additive: nothing is retired");
        assert!(
            out.contradictions.is_empty(),
            "independent objects do not conflict"
        );
    }

    #[test]
    fn opposite_booleans_contradict_and_a_high_trust_incumbent_quarantines() {
        let cfg = DetectionConfig::with_default_rules(); // boolean inversion is always on
        let subject = Id::generate();
        let cur = vec![current(&subject, "is_up", ObjectValue::Bool(true), 0.9)];
        let new = vec![mfact(&subject, "is_up", ObjectValue::Bool(false))];

        let out = run(&cur, &new, &cfg);

        assert!(
            out.supersessions.is_empty(),
            "is_up is multi-valued, not functional"
        );
        assert_eq!(out.contradictions.len(), 1, "one contradiction");
        assert!(
            out.contradictions[0].quarantine_source,
            "a high-trust pair quarantines the victim"
        );
        // The victim (the quarantined CONTRADICTS source) is the smaller object order on a
        // trust tie: object_order_key(false) < object_order_key(true), so `false` is the
        // victim — here that is the new fact, but by the symmetric rule, not by being new.
        assert_eq!(
            out.contradictions[0].source_fact.object,
            ObjectValue::Bool(false),
            "the smaller-object-order side is the victim"
        );
        assert_eq!(
            out.audits.len(),
            1,
            "the quarantine raises one reconcile signal"
        );
        assert_eq!(out.audits[0].kind, AuditKind::Quarantine);
    }

    #[test]
    fn a_symmetric_low_trust_contradiction_records_without_quarantine() {
        // Both sides below the high-trust bar: the contradiction is still recorded, the victim
        // is still chosen symmetrically (smaller object order), but neither side is quarantined
        // — max trust does not clear the threshold.
        let cfg = DetectionConfig::with_default_rules();
        let subject = Id::generate();
        let cur = vec![current(&subject, "is_up", ObjectValue::Bool(true), 0.5)];
        let new = vec![mfact_trust(
            &subject,
            "is_up",
            ObjectValue::Bool(false),
            0.5,
        )];

        let out = run(&cur, &new, &cfg);

        assert_eq!(out.contradictions.len(), 1, "still recorded");
        assert!(
            !out.contradictions[0].quarantine_source,
            "below the trust threshold neither side is quarantined"
        );
        assert_eq!(
            out.contradictions[0].source_fact.object,
            ObjectValue::Bool(false),
            "the victim is still the smaller object order, deterministically"
        );
        assert!(out.audits.is_empty(), "no quarantine, no reconcile signal");
    }

    #[test]
    fn the_lower_trust_side_is_the_victim_in_either_arrival_order() {
        // up@0.5 vs down@0.9, mutually exclusive. The lower-trust side is the victim (the
        // quarantined CONTRADICTS source) regardless of which side is the incumbent — including
        // the direction the old incumbent-keyed rule could never produce: a higher-trust
        // newcomer quarantining the lower-trust incumbent, with the audit naming the incumbent.
        let cfg = DetectionConfig::with_default_rules();
        let subject = Id::generate();

        // The low-trust `true` is the incumbent; the high-trust `false` arrives and wins.
        let incumbent = current(&subject, "is_up", ObjectValue::Bool(true), 0.5);
        let incumbent_id = incumbent.id;
        let a = detect(
            &[incumbent],
            &[mfact_trust(
                &subject,
                "is_up",
                ObjectValue::Bool(false),
                0.9,
            )],
            &cfg,
            &ns(),
            &t2(),
            &t2(),
            &Id::generate(),
        );
        assert_eq!(a.contradictions.len(), 1);
        assert_eq!(
            a.contradictions[0].source_fact.object,
            ObjectValue::Bool(true),
            "the lower-trust incumbent is the victim/source"
        );
        assert_eq!(
            a.contradictions[0].target_fact.object,
            ObjectValue::Bool(false),
            "the higher-trust newcomer survives as the target"
        );
        assert!(
            a.contradictions[0].quarantine_source,
            "max trust 0.9 clears the bar"
        );
        assert_eq!(
            a.audits[0].subject_id, incumbent_id,
            "the audit names the quarantined incumbent, not the new fact"
        );

        // Mirror arrival order: the high-trust `false` is the incumbent, the low-trust `true`
        // arrives. The same low-trust `true` is the victim, so the contradiction converges.
        let b = detect(
            &[current(&subject, "is_up", ObjectValue::Bool(false), 0.9)],
            &[mfact_trust(&subject, "is_up", ObjectValue::Bool(true), 0.5)],
            &cfg,
            &ns(),
            &t2(),
            &t2(),
            &Id::generate(),
        );
        assert_eq!(
            b.contradictions[0].source_fact.object,
            ObjectValue::Bool(true),
            "the same low-trust side is the victim in the reverse order"
        );
        assert_eq!(
            a.contradictions[0].source_fact.object, b.contradictions[0].source_fact.object,
            "the victim is identical in both arrival orders — the contradiction converges"
        );
    }

    #[test]
    fn a_configured_antonym_pair_contradicts_order_insensitively() {
        let mut cfg = DetectionConfig::with_default_rules();
        cfg.predicates.insert(
            "status".to_string(),
            PredicateRule {
                functional: false,
                contradicts: vec![(text("up"), text("down"))],
            },
        );
        let subject = Id::generate();

        let forward = run(
            &[current(&subject, "status", text("up"), 0.9)],
            &[mfact(&subject, "status", text("down"))],
            &cfg,
        );
        assert_eq!(forward.contradictions.len(), 1, "up vs down contradicts");

        let reverse = run(
            &[current(&subject, "status", text("down"), 0.9)],
            &[mfact(&subject, "status", text("up"))],
            &cfg,
        );
        assert_eq!(
            reverse.contradictions.len(),
            1,
            "down vs up contradicts too"
        );

        // The victim is identical regardless of which side arrived first: equal trust, so the
        // smaller object order ('down' < 'up') is the victim in BOTH orders. This is the
        // arrival-order-symmetry the old incumbent-keyed rule lacked.
        assert_eq!(
            forward.contradictions[0].source_fact.object,
            text("down"),
            "'down' is the victim when 'up' is the incumbent"
        );
        assert_eq!(
            reverse.contradictions[0].source_fact.object,
            text("down"),
            "'down' is still the victim when 'down' is the incumbent"
        );
    }

    #[test]
    fn the_same_triple_is_dedup_not_a_conflict() {
        let cfg = DetectionConfig::with_default_rules();
        let subject = Id::generate();
        let cur = vec![current(&subject, "based_in", text("NYC"), 0.9)];
        let new = vec![mfact(&subject, "based_in", text("NYC"))];

        let out = run(&cur, &new, &cfg);

        assert!(
            out.supersessions.is_empty(),
            "an identical object is not superseded"
        );
        assert!(
            out.contradictions.is_empty(),
            "an identical object is not a conflict"
        );
    }

    #[test]
    fn an_intra_episode_functional_tie_keeps_the_lexicographically_smallest_object() {
        let cfg = DetectionConfig::with_default_rules();
        let subject = Id::generate();
        // Two new facts for the same functional (subject, predicate) with different objects.
        // They share the episode's captured_at, so the K1 order reduces to the object order:
        // the survivor is the one whose object sorts first, decoupled from the (arrival-
        // fragile) content-hash fact id the rule used to key on.
        let nyc = mfact(&subject, "based_in", text("NYC"));
        let sf = mfact(&subject, "based_in", text("SF"));

        let out = run(&[], &[nyc.clone(), sf.clone()], &cfg);

        assert_eq!(
            out.supersessions.len(),
            1,
            "the tie yields one supersession"
        );
        let s = &out.supersessions[0];
        assert_eq!(
            s.new_fact.object,
            text("NYC"),
            "the smallest object order ('NYC' < 'SF') survives"
        );
        assert_eq!(s.old_fact.object, text("SF"), "the rest are retired by it");

        // Swapping the input order keeps the same survivor — the tiebreak is on the object,
        // not on input/arrival order.
        let swapped = run(&[], &[sf, nyc], &cfg);
        assert_eq!(swapped.supersessions.len(), 1);
        assert_eq!(
            swapped.supersessions[0].new_fact.object,
            text("NYC"),
            "survivor is independent of input order"
        );
    }

    #[test]
    fn detection_disabled_is_a_no_op() {
        let mut cfg = DetectionConfig::with_default_rules();
        cfg.enabled = false;
        let subject = Id::generate();
        let cur = vec![current(&subject, "based_in", text("NYC"), 0.9)];
        let new = vec![mfact(&subject, "based_in", text("SF"))];

        let out = run(&cur, &new, &cfg);

        assert!(out.supersessions.is_empty());
        assert!(out.contradictions.is_empty());
        assert!(out.audits.is_empty());
    }

    #[test]
    fn a_stale_assertion_is_superseded_by_a_newer_incumbent() {
        // The incumbent opens at 11:00; the "new" fact's event time is 09:00 — older. A
        // functional slot holds exactly one current object, so the stale assertion does not
        // become a second current value (the old forward-only guard's divergence): it is born
        // superseded by the newer incumbent, retained in history with a closed window.
        let cfg = DetectionConfig::with_default_rules();
        let subject = Id::generate();
        let cur = vec![CurrentFact {
            id: Id::generate(),
            key: FactKey {
                subject_id: subject,
                predicate: "based_in".to_string(),
                object: text("SF"),
            },
            valid_from: t2(),
            trust: 0.9,
        }];
        let new = vec![mfact(&subject, "based_in", text("NYC"))];

        let out = detect(&cur, &new, &cfg, &ns(), &t1(), &t1(), &Id::generate());

        assert_eq!(
            out.supersessions.len(),
            1,
            "the stale assertion is retired by the incumbent, not left additive"
        );
        let s = &out.supersessions[0];
        assert_eq!(
            s.old_fact.object,
            text("NYC"),
            "the stale new fact is the side that is superseded"
        );
        assert_eq!(
            s.new_fact.object,
            text("SF"),
            "the newer incumbent is the survivor"
        );
        assert_eq!(
            s.valid_from,
            t2(),
            "the stale fact's window closes at the incumbent's valid_from"
        );
        assert!(
            out.contradictions.is_empty(),
            "a functional supersession, not a contradiction"
        );
    }

    #[test]
    fn equal_valid_from_functional_assertions_converge_regardless_of_incumbent() {
        // Two functional assertions with the SAME event time but different objects. Whichever
        // one is the committed incumbent, the winner of the single slot is the same — the
        // smaller object order — so the outcome cannot depend on which arrived first. The
        // simultaneous-tie convergence guard at the detect level.
        let cfg = DetectionConfig::with_default_rules();
        let subject = Id::generate();

        // SF incumbent, NYC new, both at t2. 'NYC' < 'SF', so NYC wins the slot.
        let sf_incumbent = vec![current_at(&subject, "based_in", text("SF"), t2(), 0.9)];
        let nyc_new = vec![mfact(&subject, "based_in", text("NYC"))];
        let a = detect(
            &sf_incumbent,
            &nyc_new,
            &cfg,
            &ns(),
            &t2(),
            &t2(),
            &Id::generate(),
        );

        // The mirror arrival order: NYC incumbent, SF new, both at t2.
        let nyc_incumbent = vec![current_at(&subject, "based_in", text("NYC"), t2(), 0.9)];
        let sf_new = vec![mfact(&subject, "based_in", text("SF"))];
        let b = detect(
            &nyc_incumbent,
            &sf_new,
            &cfg,
            &ns(),
            &t2(),
            &t2(),
            &Id::generate(),
        );

        // In both orders the survivor is NYC and SF is the retired side — identical current
        // state regardless of which assertion happened to be committed first.
        assert_eq!(a.supersessions.len(), 1, "one supersession either way");
        assert_eq!(b.supersessions.len(), 1, "one supersession either way");
        assert_eq!(
            a.supersessions[0].new_fact.object,
            text("NYC"),
            "NYC wins when SF is the incumbent"
        );
        assert_eq!(
            b.supersessions[0].new_fact.object,
            text("NYC"),
            "NYC still wins when NYC is the incumbent (it retires the new SF)"
        );
        assert_eq!(a.supersessions[0].old_fact.object, text("SF"));
        assert_eq!(b.supersessions[0].old_fact.object, text("SF"));
    }

    #[test]
    fn one_incumbent_is_superseded_only_by_the_surviving_new_fact() {
        // An episode asserts two new values (SF, Boston) for one functional predicate while
        // an NYC incumbent stands. Every retirement must route to the single survivor — the
        // object that sorts first, 'Boston' < 'SF' — so the incumbent is retired once (not
        // once per new fact), and the losing peer is retired by that same survivor: no retired
        // fact points at anything but the winner.
        let cfg = DetectionConfig::with_default_rules();
        let subject = Id::generate();
        let sf = mfact(&subject, "based_in", text("SF"));
        let boston = mfact(&subject, "based_in", text("Boston"));
        // 'Boston' sorts before 'SF', so Boston is the survivor regardless of fact ids.
        let cur = vec![current(&subject, "based_in", text("NYC"), 0.9)];

        let out = run(&cur, &[sf.clone(), boston.clone()], &cfg);

        assert_eq!(
            out.supersessions.len(),
            2,
            "the incumbent and the losing peer are each retired exactly once"
        );
        assert!(
            out.supersessions
                .iter()
                .all(|s| s.new_fact.object == text("Boston")),
            "every retirement points at the single survivor (Boston), not a losing peer: {:?}",
            out.supersessions
        );
        let retired: Vec<ObjectValue> = out
            .supersessions
            .iter()
            .map(|s| s.old_fact.object.clone())
            .collect();
        assert!(retired.contains(&text("NYC")), "the incumbent is retired");
        assert!(retired.contains(&text("SF")), "the losing peer is retired");
        assert!(
            out.contradictions.is_empty(),
            "supersession, not contradiction"
        );
    }
}
