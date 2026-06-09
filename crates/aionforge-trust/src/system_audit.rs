//! Shared construction for substrate-authored, content-addressed audit records.
//!
//! Both the quorum promoter and the reliability scorer write `System`-namespace audit events
//! whose id is content-addressed on a `(tag, key)` pair, so a deterministic replay of the same
//! decision dedupes to a true no-op under the store's idempotent write-set. These two helpers are
//! that shared shape; each subsystem supplies its own `(tag, key)` scheme and payload.

use aionforge_domain::blocks::Identity;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::time::Timestamp;

/// A content-addressed id over `(tag, key)`, with **no time component**.
///
/// A same-decision replay computes the same id and dedupes to a no-op (the property the idempotent
/// write-set relies on). Used where a record is genuinely one-per-`(tag, key)`-lifetime and must
/// resurrect the *same* node on a re-derivation: the promotion ledger, the global fact copy, and
/// the content-idempotent consolidation audits, where re-running the same decision must never mint
/// a duplicate. A record that legitimately recurs for the same subject across cycles — a governance
/// transition or an attestation — uses [`cycle_id`] instead.
pub(crate) fn content_id(tag: &str, key: &str) -> Id {
    Id::from_content_hash(format!("{tag}|{key}").as_bytes())
}

/// A content-addressed id over `(tag, key)` plus the event's **millisecond instant** (M4.T06).
///
/// The discriminating sibling of [`content_id`]. A governance transition (promote, demote,
/// quarantine) or an attestation recurs for the same subject across cycles — a fact promoted, then
/// demoted, then re-promoted is three real events — so folding the host `now` keeps each one a
/// distinct row in the by-subject audit history. Idempotency survives: a crash-replay re-supplies
/// the *same* host instant (stored time is never an ambient clock read), so the id re-computes
/// identically and the write is still a no-op. The residual is sub-millisecond: two genuinely
/// distinct decisions on one subject within the same millisecond would collapse — not reachable for
/// the coarse-grained, reason-tagged governance transitions, and it mirrors the shipped
/// `attest_reject` id scheme.
pub(crate) fn cycle_id(tag: &str, key: &str, now: &Timestamp) -> Id {
    let millis = now.timestamp().as_millisecond();
    Id::from_content_hash(format!("{tag}|{key}|{millis}").as_bytes())
}

/// The reduced `Identity` for a substrate-authored audit/control node: the supplied
/// content-addressed id, `ingested_at = now`, the `System` namespace, and no expiry.
pub(crate) fn system_identity(id: Id, now: &Timestamp) -> Identity {
    Identity {
        id,
        ingested_at: now.clone(),
        namespace: Namespace::System,
        expired_at: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{content_id, cycle_id};
    use aionforge_domain::time::Timestamp;

    fn at(s: &str) -> Timestamp {
        s.parse().expect("valid zoned datetime")
    }

    #[test]
    fn cycle_id_distinguishes_distinct_instants() {
        let t1 = at("2026-06-08T09:00:00-05:00[America/Chicago]");
        let t2 = at("2026-06-08T09:00:01-05:00[America/Chicago]");
        assert_ne!(
            cycle_id("promote", "fact-x", &t1),
            cycle_id("promote", "fact-x", &t2),
            "a genuine recurrence at a later instant is a distinct event"
        );
    }

    #[test]
    fn cycle_id_dedups_a_same_instant_replay() {
        let t = at("2026-06-08T09:00:00-05:00[America/Chicago]");
        assert_eq!(
            cycle_id("promote", "fact-x", &t),
            cycle_id("promote", "fact-x", &t),
            "a crash-replay re-supplies the same instant and re-computes the same id"
        );
    }

    #[test]
    fn the_same_instant_restated_in_another_zone_is_one_event() {
        // 14:00Z and 09:00-05:00 are the same absolute instant; the discriminator folds the
        // millisecond instant, so a replay that re-states the time in a different zone still dedups.
        let z1 = at("2026-06-08T14:00:00+00:00[UTC]");
        let z2 = at("2026-06-08T09:00:00-05:00[America/Chicago]");
        assert_eq!(cycle_id("promote", "f", &z1), cycle_id("promote", "f", &z2));
    }

    #[test]
    fn cycle_id_and_content_id_do_not_alias() {
        // Moving the governance family to cycle_id can never collide with a content_id record of
        // the same (tag, key) — the promotion ledger, the global copy — since the schemes hash
        // different byte strings.
        let t = at("2026-06-08T09:00:00-05:00[America/Chicago]");
        assert_ne!(content_id("promote", "f"), cycle_id("promote", "f", &t));
    }

    #[test]
    fn a_distinct_tag_or_key_stays_distinct() {
        let t = at("2026-06-08T09:00:00-05:00[America/Chicago]");
        assert_ne!(cycle_id("promote", "f", &t), cycle_id("demote", "f", &t));
        assert_ne!(cycle_id("promote", "f", &t), cycle_id("promote", "g", &t));
    }
}
