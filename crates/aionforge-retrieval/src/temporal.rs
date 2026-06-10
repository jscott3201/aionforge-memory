//! The bi-temporal fact predicate and the fact serialization id (03 §5–§6).
//!
//! These are the pure functions behind bi-temporal retrieval, kept apart from the
//! retriever's I/O so they can be reasoned about and unit-tested in isolation. A fact
//! candidate is admitted by [`fact_passes_temporal`] against the query's
//! [`TemporalMode`], and rendered under the content-derived id from
//! [`fact_serialization_id`] — keyed on the triple identity, not the mint-time domain
//! id, so the rendered view stays byte-identical across re-extraction and rebuilds (the
//! prefix-cache contract). The window comparisons are done in Rust because selene-db
//! cannot index a zoned datetime, so the candidate set is bounded by search first and
//! filtered by instant here.

use aionforge_domain::edges::About;
use aionforge_domain::ids::SerializationId;
use aionforge_domain::nodes::semantic::{Fact, FactStatus};

use crate::query::TemporalMode;

/// The serialization-id kind tag for a fact (02 §10). Namespacing the hash keeps a fact
/// and an episode whose canonical keys collide from sharing a serialization id.
const FACT_KIND_TAG: &str = "fact";

/// The unit separator joining a fact's triple parts into its serialization key. A
/// control character keeps a predicate that contains the delimiter from colliding with a
/// different (subject, predicate, object) split.
const FACT_KEY_SEP: char = '\u{1f}';

/// Whether a fact passes the query's bi-temporal mode (03 §5). The candidate set is
/// already bounded by search (and, in [`TemporalMode::Current`], scoped to the
/// `current_support_facts` provider), so this is the per-candidate window test over the
/// fact's `ABOUT` validity block.
///
/// - `Current`: the provider already removed superseded/contradicted facts structurally;
///   this applies the `status == active` scalar half the provider cannot express (02 §9).
/// - `AsOf(t)`: event time — true at `t` iff `valid_from <= t < valid_to` (open `valid_to`
///   is unbounded above).
/// - `AsKnownAt(t)`: transaction time — believed at `t` iff `ingested_at <= t < expired_at`
///   (open `expired_at` is unbounded above).
/// - `History`: every status and window passes.
pub(crate) fn fact_passes_temporal(mode: &TemporalMode, fact: &Fact, about: &About) -> bool {
    match mode {
        TemporalMode::Current => fact.status == FactStatus::Active,
        TemporalMode::AsOf(t) => {
            about.temporal.valid_from <= *t
                && about.temporal.valid_to.as_ref().is_none_or(|to| *t < *to)
        }
        TemporalMode::AsKnownAt(t) => {
            about.temporal.ingested_at <= *t
                && about.temporal.expired_at.as_ref().is_none_or(|to| *t < *to)
        }
        TemporalMode::History => true,
    }
}

/// The content-derived serialization id for a fact: a hash over its triple identity
/// (subject, predicate, canonical object), not its mint-time domain id. Two assertions of
/// the same triple render under the same id, so the rendered view stays stable across
/// re-extraction and rebuilds — the prefix-cache contract (03 §6).
pub(crate) fn fact_serialization_id(fact: &Fact) -> SerializationId {
    // The object is rendered through its canonical JSON so the tag and value are part of
    // the key (an entity-id object never collides with the same id as a text literal).
    let object = serde_json::to_string(&fact.object).unwrap_or_default();
    let key = format!(
        "{subject}{sep}{predicate}{sep}{object}",
        subject = fact.subject_id,
        sep = FACT_KEY_SEP,
        predicate = fact.predicate,
    );
    SerializationId::derive(FACT_KIND_TAG, key.as_bytes())
}

#[cfg(test)]
mod tests {
    use aionforge_domain::blocks::{Identity, Stats};
    use aionforge_domain::ids::Id;
    use aionforge_domain::namespace::Namespace;
    use aionforge_domain::time::{BiTemporal, Timestamp};
    use aionforge_domain::value::ObjectValue;

    use super::*;

    fn ts(text: &str) -> Timestamp {
        text.parse().expect("valid zoned datetime")
    }

    fn fact(subject: &str, predicate: &str, object: ObjectValue, status: FactStatus) -> Fact {
        Fact {
            identity: Identity {
                id: Id::generate(),
                ingested_at: ts("2026-06-06T00:00:00Z[UTC]"),
                namespace: Namespace::Global,
                expired_at: None,
            },
            stats: Stats {
                importance: 0.5,
                trust: 0.8,
                last_access: ts("2026-06-06T00:00:00Z[UTC]"),
                access_count_recent: 0,
                referenced_count: 0,
                surprise: 0.0,
                is_pinned: false,
            },
            subject_id: Id::from_content_hash(subject.as_bytes()),
            predicate: predicate.to_string(),
            object,
            confidence: 0.9,
            status,
            statement: format!("{subject} {predicate}"),
            embedding: None,
            embedder_model: None,
            extraction: None,
            cooled_until: None,
        }
    }

    fn about(
        valid_from: &str,
        valid_to: Option<&str>,
        ingested: &str,
        expired: Option<&str>,
    ) -> About {
        About {
            temporal: BiTemporal {
                valid_from: ts(valid_from),
                valid_to: valid_to.map(ts),
                ingested_at: ts(ingested),
                expired_at: expired.map(ts),
            },
        }
    }

    #[test]
    fn fact_serialization_id_is_a_pure_function_of_the_triple() {
        // The serialization id keys on (subject, predicate, object), not the mint-time
        // domain id, so two distinct assertions of the same triple share it.
        let a = fact(
            "acme",
            "based_in",
            ObjectValue::Text("NYC".into()),
            FactStatus::Active,
        );
        let b = fact(
            "acme",
            "based_in",
            ObjectValue::Text("NYC".into()),
            FactStatus::Superseded,
        );
        assert_ne!(a.identity.id, b.identity.id, "distinct domain ids");
        assert_eq!(
            fact_serialization_id(&a),
            fact_serialization_id(&b),
            "same triple renders under the same serialization id",
        );
    }

    #[test]
    fn fact_serialization_id_separates_distinct_triples() {
        let nyc = fact(
            "acme",
            "based_in",
            ObjectValue::Text("NYC".into()),
            FactStatus::Active,
        );
        let sf = fact(
            "acme",
            "based_in",
            ObjectValue::Text("SF".into()),
            FactStatus::Active,
        );
        let other = fact(
            "acme",
            "founded_in",
            ObjectValue::Text("NYC".into()),
            FactStatus::Active,
        );
        assert_ne!(
            fact_serialization_id(&nyc),
            fact_serialization_id(&sf),
            "object differs"
        );
        assert_ne!(
            fact_serialization_id(&nyc),
            fact_serialization_id(&other),
            "predicate differs",
        );
    }

    #[test]
    fn current_mode_keeps_only_active_facts() {
        let active = fact("a", "p", ObjectValue::Text("o".into()), FactStatus::Active);
        let superseded = fact(
            "a",
            "p",
            ObjectValue::Text("o".into()),
            FactStatus::Superseded,
        );
        let window = about(
            "2026-01-01T00:00:00Z[UTC]",
            None,
            "2026-01-01T00:00:00Z[UTC]",
            None,
        );
        assert!(fact_passes_temporal(
            &TemporalMode::Current,
            &active,
            &window
        ));
        assert!(!fact_passes_temporal(
            &TemporalMode::Current,
            &superseded,
            &window
        ));
    }

    #[test]
    fn as_of_reads_the_event_window_half_open() {
        let f = fact(
            "a",
            "p",
            ObjectValue::Text("o".into()),
            FactStatus::Superseded,
        );
        // Valid in the world over [T1, T2); the status is irrelevant to as-of.
        let window = about(
            "2026-01-01T00:00:00Z[UTC]",
            Some("2026-06-01T00:00:00Z[UTC]"),
            "2026-01-01T00:00:00Z[UTC]",
            None,
        );
        assert!(
            fact_passes_temporal(
                &TemporalMode::AsOf(ts("2026-03-01T00:00:00Z[UTC]")),
                &f,
                &window
            ),
            "inside the window",
        );
        assert!(
            !fact_passes_temporal(
                &TemporalMode::AsOf(ts("2025-12-01T00:00:00Z[UTC]")),
                &f,
                &window
            ),
            "before valid_from",
        );
        assert!(
            !fact_passes_temporal(
                &TemporalMode::AsOf(ts("2026-06-01T00:00:00Z[UTC]")),
                &f,
                &window
            ),
            "valid_to is exclusive",
        );
    }

    #[test]
    fn as_known_at_reads_the_transaction_window_half_open() {
        let f = fact("a", "p", ObjectValue::Text("o".into()), FactStatus::Active);
        // Believed over [T1, T2) in transaction time, regardless of event validity.
        let window = about(
            "2020-01-01T00:00:00Z[UTC]",
            None,
            "2026-01-01T00:00:00Z[UTC]",
            Some("2026-06-01T00:00:00Z[UTC]"),
        );
        assert!(fact_passes_temporal(
            &TemporalMode::AsKnownAt(ts("2026-03-01T00:00:00Z[UTC]")),
            &f,
            &window,
        ));
        assert!(!fact_passes_temporal(
            &TemporalMode::AsKnownAt(ts("2025-12-01T00:00:00Z[UTC]")),
            &f,
            &window,
        ));
        assert!(
            !fact_passes_temporal(
                &TemporalMode::AsKnownAt(ts("2026-06-01T00:00:00Z[UTC]")),
                &f,
                &window,
            ),
            "expired_at is exclusive",
        );
    }

    #[test]
    fn history_keeps_every_status_and_window() {
        let superseded = fact(
            "a",
            "p",
            ObjectValue::Text("o".into()),
            FactStatus::Superseded,
        );
        let closed = about(
            "2026-01-01T00:00:00Z[UTC]",
            Some("2026-02-01T00:00:00Z[UTC]"),
            "2026-01-01T00:00:00Z[UTC]",
            Some("2026-02-01T00:00:00Z[UTC]"),
        );
        assert!(fact_passes_temporal(
            &TemporalMode::History,
            &superseded,
            &closed
        ));
    }
}
