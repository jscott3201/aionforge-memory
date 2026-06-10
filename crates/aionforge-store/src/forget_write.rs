//! Soft-forget / unforget write primitives (05 §2, M5.T02).
//!
//! Both writes flip exactly one thing — `Identity.expired_at` on the node — and co-commit
//! their audit row through the single [`crate::audit::ensure_event`] funnel in the same
//! transaction, modeled on the demotion write minus the lineage edge and minus the status
//! flip. Status is deliberately untouched: a soft-forgotten memory keeps `status: Active`,
//! which is what distinguishes it from the demotion quarantine (`expired_at` paired with
//! `Quarantined`), keeps a fact in the current-support provider set, and leaves the
//! retrieval gate as the single exclusion mechanism. No edge is written or closed, so
//! `ATTESTED_BY` and every other relationship survive a soft-forget untouched (the M5.T03
//! erasure boundary).
//!
//! Idempotency is probed under the write lock: a write happens only on a real state
//! transition, and the audit row is emitted only with it — so a replay is a true no-op
//! with no second audit, and the caller's `cycle_id`-addressed events stay one row per
//! decision.
//!
//! The non-`Active`-status refusal extends the design's unforget-only quarantine guard
//! to both directions deliberately: a contradiction quarantine leaves `expired_at`
//! unset, so without the forward guard a soft-forget could land on a quarantined node
//! and manufacture the demotion signature — collapsing two channels the four-signature
//! table (on the domain predicate's doc) keeps distinct. The extension can only refuse
//! writes, never widen them.

use aionforge_domain::edges::Audit;
use aionforge_domain::nodes::forensic::AuditEvent;
use aionforge_domain::nodes::semantic::FactStatus;
use aionforge_domain::time::Timestamp;
use selene_core::{LabelDiff, PropertyDiff, PropertyMap, db_string};

use crate::convert::{enum_from_value, key, timestamp_value};
use crate::error::StoreError;
use crate::store::Store;
use crate::{NodeId, audit};

const EXPIRED_AT: &str = "expired_at";
const STATUS: &str = "status";

/// The outcome of a soft-forget or unforget write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgetWrite {
    /// The state flipped and the audit row was co-committed.
    Applied,
    /// Already in the target state. Nothing was written and no audit row was emitted —
    /// a crash-replay or double call converges instead of minting a second event.
    Noop,
    /// The node's `status` marks it as another revision channel's territory
    /// (quarantined or superseded), so the write was refused. Soft-forget only ever
    /// produces the bare-`expired_at`-with-`Active`-status signature; writing it over a
    /// non-`Active` node would manufacture an ambiguous fifth lifecycle signature, and
    /// un-forgetting one would resurrect state another channel retired.
    RefusedStatus,
}

impl Store {
    /// Soft-forget a memory: set `Identity.expired_at = now`, leave `status` and every
    /// edge untouched, and co-commit the caller's `Forget` audit (05 §2, M5.T02).
    ///
    /// The probe and the write share one lock: if `expired_at` is already set (a prior
    /// soft-forget or a demotion) this is a [`ForgetWrite::Noop`]; if the node carries a
    /// non-`Active` status it is [`ForgetWrite::RefusedStatus`]. The audit edge is wired
    /// only when the event row was actually created, so replays converge.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the node has no properties or a read/write fails.
    pub fn soft_forget(
        &self,
        node: NodeId,
        now: &Timestamp,
        audit: &AuditEvent,
    ) -> Result<ForgetWrite, StoreError> {
        self.flip_expired_at(node, Some(now), audit)
    }

    /// Reverse a soft-forget: clear `Identity.expired_at`, leave `status` and every edge
    /// untouched, and co-commit the caller's `Unforget` audit (05 §2, M5.T02).
    ///
    /// A node with no `expired_at` is a [`ForgetWrite::Noop`] — including a quarantined
    /// or superseded node that was never expired, since it is already in the cleared
    /// state. A node carrying an expiry *paired with* a non-`Active` status (the demotion
    /// shape) is [`ForgetWrite::RefusedStatus`]: that expiry belongs to governance, and
    /// clearing it here would resurrect a fact it retired — re-promotion owns that path.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the node has no properties or a read/write fails.
    pub fn unforget(&self, node: NodeId, audit: &AuditEvent) -> Result<ForgetWrite, StoreError> {
        self.flip_expired_at(node, None, audit)
    }

    /// The shared flip: set (`Some(now)`) or clear (`None`) `expired_at`, gated on a real
    /// transition and on an `Active`-or-statusless node, audit co-committed.
    fn flip_expired_at(
        &self,
        node: NodeId,
        set_to: Option<&Timestamp>,
        audit_event: &AuditEvent,
    ) -> Result<ForgetWrite, StoreError> {
        let expired_key = db_string(EXPIRED_AT)?;
        let status_key = db_string(STATUS)?;
        let audit_edge = db_string(Audit::LABEL)?;

        let mut txn = self.graph().begin_write();
        let outcome = {
            let mut mutator = txn.mutator();
            // Probe under the write lock so the gate and the write are atomic.
            let (already_expired, status) = {
                let props = mutator.read().node_properties(node).ok_or_else(|| {
                    StoreError::invariant("forget write target has no properties".to_string())
                })?;
                (
                    props.get(&expired_key).is_some(),
                    props.get(&status_key).cloned(),
                )
            };
            let status_is_active = match status {
                None => true,
                Some(value) => enum_from_value::<FactStatus>(&value)? == FactStatus::Active,
            };
            if already_expired == set_to.is_some() {
                ForgetWrite::Noop
            } else if !status_is_active {
                ForgetWrite::RefusedStatus
            } else {
                let diff = match set_to {
                    Some(now) => PropertyDiff::new([(key(EXPIRED_AT)?, timestamp_value(now))], [])?,
                    None => PropertyDiff::new([], [key(EXPIRED_AT)?])?,
                };
                mutator.update_node(node, LabelDiff::new([], [])?, diff)?;
                let ensured = audit::ensure_event(&mut mutator, audit_event, self.audit_signer())?;
                if ensured.created {
                    mutator.create_edge(
                        audit_edge,
                        ensured.node,
                        node,
                        PropertyMap::from_pairs(Vec::new())?,
                    )?;
                }
                ForgetWrite::Applied
            }
        };
        txn.commit()?;
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    //! The byte-identical round-trip needs the raw property map, which has no public
    //! surface — everything else about the writes is covered on the public surface in
    //! `tests/forget_write.rs`.

    use aionforge_domain::blocks::{Identity, Stats};
    use aionforge_domain::ids::Id;
    use aionforge_domain::namespace::Namespace;
    use aionforge_domain::nodes::forensic::AuditKind;
    use aionforge_domain::nodes::semantic::{Fact, FactStatus};
    use aionforge_domain::value::ObjectValue;
    use selene_core::PropertyMap;

    use super::*;
    use crate::config::StoreConfig;

    fn ts(text: &str) -> Timestamp {
        text.parse().expect("valid zoned datetime literal")
    }

    fn audit_event(kind: AuditKind, subject: Id, seed: &[u8]) -> AuditEvent {
        AuditEvent {
            identity: Identity {
                id: Id::from_content_hash(seed),
                ingested_at: ts("2026-06-06T12:00:00-05:00[America/Chicago]"),
                namespace: Namespace::Global,
                expired_at: None,
            },
            kind,
            subject_id: subject,
            actor_id: Id::from_content_hash(b"test-sweeper"),
            payload: serde_json::json!({"reason": "test"}),
            signature: String::new(),
            occurred_at: ts("2026-06-06T12:00:00-05:00[America/Chicago]"),
        }
    }

    #[test]
    fn forget_then_unforget_restores_the_exact_property_map() {
        let store = Store::open_with_config(StoreConfig {
            embedding_dimension: 4,
        })
        .expect("open store");
        store
            .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
            .expect("migrate");
        let fact = Fact {
            identity: Identity {
                id: Id::generate(),
                ingested_at: ts("2026-06-01T09:00:00-05:00[America/Chicago]"),
                namespace: Namespace::Global,
                expired_at: None,
            },
            stats: Stats {
                importance: 0.04,
                trust: 0.2,
                last_access: ts("2026-06-01T09:00:00-05:00[America/Chicago]"),
                access_count_recent: 0,
                referenced_count: 0,
                surprise: 0.1,
                is_pinned: false,
            },
            subject_id: Id::from_content_hash(b"subject"),
            predicate: "tests".to_string(),
            object: ObjectValue::Text("round-trip".to_string()),
            confidence: 0.9,
            status: FactStatus::Active,
            statement: "tests round-trip".to_string(),
            embedding: None,
            embedder_model: None,
            extraction: None,
        };
        let node = store.insert_fact(&fact).expect("insert");
        let props_of = || -> PropertyMap {
            store
                .graph()
                .read()
                .node_properties(node)
                .expect("props")
                .clone()
        };
        let original = props_of();
        let now = ts("2026-06-06T12:00:00-05:00[America/Chicago]");

        let forgotten = store
            .soft_forget(
                node,
                &now,
                &audit_event(AuditKind::Forget, fact.identity.id, b"rt-forget"),
            )
            .expect("forget");
        assert_eq!(forgotten, ForgetWrite::Applied);
        let mid = props_of();
        let expired_key = db_string(EXPIRED_AT).unwrap();
        assert!(mid.get(&expired_key).is_some());
        // Exactly one key differs: remove it and the rest must equal the original —
        // status included (it is untouched, the signature that separates soft-forget
        // from demotion).
        let mid_without_expiry = PropertyMap::from_pairs(
            mid.iter()
                .filter(|(name, _)| **name != expired_key)
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect::<Vec<_>>(),
        )
        .expect("rebuild map");
        assert_eq!(
            mid_without_expiry, original,
            "the flip touches expired_at and nothing else"
        );

        let restored = store
            .unforget(
                node,
                &audit_event(AuditKind::Unforget, fact.identity.id, b"rt-unforget"),
            )
            .expect("unforget");
        assert_eq!(restored, ForgetWrite::Applied);
        assert_eq!(
            props_of(),
            original,
            "unforget restores the byte-identical property map"
        );
    }
}
