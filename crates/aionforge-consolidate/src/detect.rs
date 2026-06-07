//! Supersession / contradiction detection (write-and-consolidation §2, M2.T05b).
//!
//! A pure function over the committed current facts and the episode's newly extracted
//! facts: it decides, conservatively and deterministically, which new facts supersede a
//! prior one (functional predicate, newer different object) and which contradict one
//! (mutually-exclusive object), and whether a contradiction should quarantine the new
//! fact (a high-trust incumbent). It produces only INSTRUCTIONS — the store materializes
//! them in the flip transaction. Being store-free, every branch is unit-testable.

use std::collections::BTreeMap;

use aionforge_domain::blocks::Identity;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::semantic::Fact;
use aionforge_domain::time::Timestamp;
use aionforge_domain::value::ObjectValue;
use aionforge_store::{Contradiction, FactKey, MaterializedFact, Supersession};
use serde_json::json;

use crate::config::DetectionConfig;

/// A committed current fact (no live supersession/contradiction), projected for detection.
pub(crate) struct CurrentFact {
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

    // New facts vs the committed current set, scoped to the same (subject, predicate).
    for materialized in new_facts {
        let new_key = fact_key(&materialized.fact);
        let rule = cfg.rule(&new_key.predicate);
        for incumbent in current.iter().filter(|c| {
            c.key.subject_id == new_key.subject_id && c.key.predicate == new_key.predicate
        }) {
            if incumbent.key.object == new_key.object {
                continue; // the same triple — T04a dedup handles it, not a conflict
            }
            if rule.functional && *captured_at >= incumbent.valid_from {
                out.supersessions.push(Supersession {
                    old_fact: incumbent.key.clone(),
                    new_fact: new_key.clone(),
                    reason: "functional predicate superseded by a newer assertion".to_string(),
                    valid_from: captured_at.clone(),
                });
            } else if mutually_exclusive(&rule, &incumbent.key.object, &new_key.object) {
                let quarantine = incumbent.trust >= cfg.high_trust_threshold;
                out.contradictions.push(Contradiction {
                    source_fact: new_key.clone(),
                    target_fact: incumbent.key.clone(),
                    detected_by: "detection-v1".to_string(),
                    quarantine_source: quarantine,
                    detected_at: captured_at.clone(),
                });
                if quarantine {
                    out.audits.push(quarantine_audit(
                        namespace,
                        &materialized.fact,
                        incumbent,
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

/// Among new facts that share a functional `(subject, predicate)`, keep the one with the
/// lowest content-hash fact id and supersede the rest by it — a deterministic, clock-free
/// tiebreak for the within-episode case (both facts share `captured_at`).
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
                .entry((key.subject_id.as_str().to_string(), key.predicate))
                .or_default()
                .push(materialized);
        }
    }
    for (_, mut group) in groups {
        if group.len() < 2 {
            continue;
        }
        group.sort_by(|a, b| a.fact.identity.id.as_str().cmp(b.fact.identity.id.as_str()));
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
        subject_id: fact.subject_id.clone(),
        predicate: fact.predicate.clone(),
        object: fact.object.clone(),
    }
}

/// The `quarantine` reconcile-signal audit event (the spec's surfaced signal).
fn quarantine_audit(
    namespace: &Namespace,
    new_fact: &Fact,
    incumbent: &CurrentFact,
    now: &Timestamp,
    actor_id: &Id,
) -> AuditEvent {
    AuditEvent {
        identity: Identity {
            id: Id::generate(),
            ingested_at: now.clone(),
            namespace: namespace.clone(),
            expired_at: None,
        },
        kind: AuditKind::Quarantine,
        subject_id: new_fact.identity.id.clone(),
        actor_id: actor_id.clone(),
        payload: json!({
            "predicate": new_fact.predicate,
            "new_object": new_fact.object,
            "incumbent_object": incumbent.key.object,
            "incumbent_trust": incumbent.trust,
            "reason": "new fact contradicts a high-trust current fact",
        }),
        signature: String::new(),
        occurred_at: now.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionforge_domain::blocks::Stats;
    use aionforge_domain::edges::About;
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
        CurrentFact {
            key: FactKey {
                subject_id: subject.clone(),
                predicate: predicate.to_string(),
                object,
            },
            valid_from: t1(),
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
                subject_id: subject.clone(),
                predicate: predicate.to_string(),
                object,
                confidence: 0.9,
                status: FactStatus::Active,
                statement: String::new(),
                embedding: None,
                embedder_model: None,
                extraction: None,
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
            "a high-trust incumbent quarantines the new fact"
        );
        assert_eq!(
            out.audits.len(),
            1,
            "the quarantine raises one reconcile signal"
        );
        assert_eq!(out.audits[0].kind, AuditKind::Quarantine);
    }

    #[test]
    fn a_low_trust_incumbent_records_the_contradiction_without_quarantine() {
        let cfg = DetectionConfig::with_default_rules();
        let subject = Id::generate();
        let cur = vec![current(&subject, "is_up", ObjectValue::Bool(true), 0.5)];
        let new = vec![mfact(&subject, "is_up", ObjectValue::Bool(false))];

        let out = run(&cur, &new, &cfg);

        assert_eq!(out.contradictions.len(), 1, "still recorded");
        assert!(
            !out.contradictions[0].quarantine_source,
            "below the trust threshold the new fact is not quarantined"
        );
        assert!(out.audits.is_empty(), "no quarantine, no reconcile signal");
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
    fn an_intra_episode_functional_tie_keeps_the_lowest_id() {
        let cfg = DetectionConfig::with_default_rules();
        let subject = Id::generate();
        // Two new facts for the same functional (subject, predicate) with different objects.
        let a = mfact_with_id(
            Id::from_content_hash(b"a"),
            &subject,
            "based_in",
            text("NYC"),
        );
        let b = mfact_with_id(
            Id::from_content_hash(b"b"),
            &subject,
            "based_in",
            text("SF"),
        );
        let (survivor, loser) = if a.fact.identity.id.as_str() < b.fact.identity.id.as_str() {
            (&a, &b)
        } else {
            (&b, &a)
        };

        let out = run(&[], &[a.clone(), b.clone()], &cfg);

        assert_eq!(
            out.supersessions.len(),
            1,
            "the tie yields one supersession"
        );
        let s = &out.supersessions[0];
        assert_eq!(
            s.new_fact.object, survivor.fact.object,
            "lowest id survives"
        );
        assert_eq!(
            s.old_fact.object, loser.fact.object,
            "the rest are retired by it"
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
    fn a_stale_assertion_does_not_supersede_a_newer_incumbent() {
        // The incumbent opens at 11:00; the "new" fact's event time is 09:00 — older. A
        // functional predicate only supersedes forward in event time, so this is additive.
        let cfg = DetectionConfig::with_default_rules();
        let subject = Id::generate();
        let cur = vec![CurrentFact {
            key: FactKey {
                subject_id: subject.clone(),
                predicate: "based_in".to_string(),
                object: text("SF"),
            },
            valid_from: t2(),
            trust: 0.9,
        }];
        let new = vec![mfact(&subject, "based_in", text("NYC"))];

        let out = detect(&cur, &new, &cfg, &ns(), &t1(), &t1(), &Id::generate());

        assert!(
            out.supersessions.is_empty(),
            "a stale assertion cannot retire a newer incumbent"
        );
    }
}
