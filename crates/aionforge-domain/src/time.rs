//! The canonical timestamp type and the bi-temporal four-timestamp block.

use serde::{Deserialize, Serialize};

/// The canonical timestamp type.
///
/// Maps to selene-db's `ZONED DATETIME`: nanosecond resolution with a real IANA
/// time zone, carried by [`jiff::Zoned`]. The storage layer translates to and from
/// the engine's value at the boundary.
pub type Timestamp = jiff::Zoned;

/// The same instant restated in UTC.
///
/// Timestamps that cross host boundaries inside signed or pinned artifacts (audit
/// `KeyRotation` windows, the keyring file) are stamped in UTC: an RFC 9557 string
/// with an IANA zone annotation is re-checked against the *reader's* tz database on
/// parse, so a zone-bracketed stamp can fail to parse on an untampered artifact after
/// a tzdb revision. UTC has no rules to revise, which makes that conflict unreachable.
#[must_use]
pub fn to_utc(at: &Timestamp) -> Timestamp {
    at.timestamp().to_zoned(jiff::tz::TimeZone::UTC)
}

/// The four-timestamp validity block carried by every bi-temporal edge (02 §5).
///
/// Event time (`valid_from`/`valid_to`) records when the underlying fact was true
/// in the world; transaction time (`ingested_at`/`expired_at`) records when the
/// substrate believed it. An open (`None`) upper bound means "still in effect".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BiTemporal {
    /// Event-time lower bound: when the fact became true.
    pub valid_from: Timestamp,
    /// Event-time upper bound: when it stopped being true; `None` while current.
    pub valid_to: Option<Timestamp>,
    /// Transaction-time lower bound: when the substrate recorded it (immutable).
    pub ingested_at: Timestamp,
    /// Transaction-time upper bound: when the record was expired; `None` while live.
    pub expired_at: Option<Timestamp>,
}

impl BiTemporal {
    /// True when the record is currently live in transaction time (`expired_at` is open).
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.expired_at.is_none()
    }

    /// True when the fact is currently valid in event time (`valid_to` is open).
    #[must_use]
    pub fn is_current(&self) -> bool {
        self.valid_to.is_none()
    }

    /// True when both windows are well-ordered: neither the event-time nor the
    /// transaction-time lower bound sits after its (present) upper bound.
    ///
    /// The core bi-temporal invariant. An open (`None`) upper bound is vacuously
    /// ordered. Every window the write path produces must satisfy this — closing a
    /// window (e.g. supersession setting `valid_to`) must not place the bound before
    /// `valid_from`.
    #[must_use]
    pub fn windows_ordered(&self) -> bool {
        self.valid_to
            .as_ref()
            .is_none_or(|to| self.valid_from <= *to)
            && self
                .expired_at
                .as_ref()
                .is_none_or(|to| self.ingested_at <= *to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> Timestamp {
        jiff::Timestamp::new(secs, 0)
            .expect("valid instant")
            .to_zoned(jiff::tz::TimeZone::UTC)
    }

    #[test]
    fn to_utc_keeps_the_instant_and_drops_the_zone_rules() {
        let chicago: Timestamp = "2026-06-09T10:00:00-05:00[America/Chicago]"
            .parse()
            .expect("valid zoned datetime");
        let utc = to_utc(&chicago);
        assert_eq!(utc.timestamp(), chicago.timestamp());
        let wire = serde_json::to_string(&utc).expect("serializes");
        assert!(wire.contains("+00:00[UTC]"), "stamped in UTC, got {wire}");
    }

    #[test]
    fn open_windows_are_live_current_and_ordered() {
        let open = BiTemporal {
            valid_from: at(100),
            valid_to: None,
            ingested_at: at(100),
            expired_at: None,
        };
        assert!(open.is_live());
        assert!(open.is_current());
        assert!(open.windows_ordered());
    }

    #[test]
    fn a_closed_window_with_an_upper_bound_before_its_lower_bound_is_not_ordered() {
        let backwards = BiTemporal {
            valid_from: at(200),
            valid_to: Some(at(100)),
            ingested_at: at(200),
            expired_at: None,
        };
        assert!(!backwards.windows_ordered());
        assert!(!backwards.is_current(), "a set valid_to means not current");

        let ok = BiTemporal {
            valid_from: at(100),
            valid_to: Some(at(200)),
            ingested_at: at(100),
            expired_at: Some(at(200)),
        };
        assert!(ok.windows_ordered());
        assert!(!ok.is_live(), "a set expired_at means not live");
    }
}
