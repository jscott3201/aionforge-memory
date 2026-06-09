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

/// A content-addressed id over `(tag, key)`, with **no cycle component**.
///
/// A same-decision replay computes the same id and dedupes to a no-op (the property the idempotent
/// write-set relies on). The trade-off: a second *genuine* decision over the same `(tag, key)` in a
/// later cycle computes the same id and writes nothing, so the audit subgraph holds one event per
/// `(tag, key)` lifetime, not per cycle. A deterministic cycle discriminator lands with the M4.T06
/// by-subject audit-history consumer that actually reads this back.
pub(crate) fn content_id(tag: &str, key: &str) -> Id {
    Id::from_content_hash(format!("{tag}|{key}").as_bytes())
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
