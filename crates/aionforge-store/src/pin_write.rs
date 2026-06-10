//! Pin / unpin write primitives (05 §2, M5.T02 rider).
//!
//! Both writes flip exactly one thing — `Stats.is_pinned` on the node — and co-commit
//! their audit row through the single [`crate::audit::ensure_event`] funnel in the same
//! transaction, the same shape as the soft-forget flip. Idempotency is probed under the
//! write lock: a write happens only on a real state transition, and the audit row is
//! emitted only with it, so a replay or double call converges with no second event.
//!
//! There is deliberately **no status guard** here, where the forget flip refuses a
//! non-`Active` status. That refusal exists because `expired_at` is *shared* with the
//! demotion channel — writing it over a quarantined node would manufacture the demotion
//! signature and collapse two rows of the four-signature lifecycle table (see the domain
//! decay module). `is_pinned` is shared with **no** revision channel: only the per-kind
//! insert serializers write it, and only decay and the forgetting gates read it, so
//! there is no signature to corrupt and nothing another channel owns. A quarantined or
//! superseded memory may be pinned — the pin is a retention preference, not a lifecycle
//! transition. Likewise `expired_at` is never read or written here: pinning a
//! soft-forgotten memory protects it without restoring it (un-forgetting is its own
//! audited transition).

use aionforge_domain::edges::Audit;
use aionforge_domain::nodes::forensic::AuditEvent;
use selene_core::{LabelDiff, PropertyDiff, PropertyMap, Value, db_string};

use crate::convert::key;
use crate::error::StoreError;
use crate::store::Store;
use crate::{NodeId, audit};

const IS_PINNED: &str = "is_pinned";

/// The outcome of a pin or unpin write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinWrite {
    /// The state flipped and the audit row was co-committed.
    Applied,
    /// Already in the target state. Nothing was written and no audit row was emitted —
    /// a crash-replay or double call converges instead of minting a second event.
    Noop,
}

impl Store {
    /// Pin a memory: set `Stats.is_pinned = true` and co-commit the caller's `Pin`
    /// audit (05 §2). The pin holds the memory at full write-time importance in every
    /// ranking and spares it from every forgetting path; status, expiry, and every edge
    /// are untouched.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the node has no properties or a read/write fails.
    pub fn set_pinned(&self, node: NodeId, audit: &AuditEvent) -> Result<PinWrite, StoreError> {
        self.flip_is_pinned(node, true, audit)
    }

    /// Lift a pin: set `Stats.is_pinned = false` and co-commit the caller's `Unpin`
    /// audit (05 §2). The memory re-enters decay and sweep eligibility — a stay, not a
    /// vault — and is forgotten later only if every eligibility axis independently
    /// holds low.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the node has no properties or a read/write fails.
    pub fn clear_pinned(&self, node: NodeId, audit: &AuditEvent) -> Result<PinWrite, StoreError> {
        self.flip_is_pinned(node, false, audit)
    }

    /// The shared flip: write `is_pinned = set_to`, gated on a real transition, audit
    /// co-committed. The column is `NOT NULL DEFAULT FALSE` on every `Stats`-bearing
    /// kind, so the flip always sets a value and never removes the key.
    fn flip_is_pinned(
        &self,
        node: NodeId,
        set_to: bool,
        audit_event: &AuditEvent,
    ) -> Result<PinWrite, StoreError> {
        let pinned_key = db_string(IS_PINNED)?;
        let audit_edge = db_string(Audit::LABEL)?;

        let mut txn = self.graph().begin_write();
        let outcome = {
            let mut mutator = txn.mutator();
            // Probe under the write lock so the gate and the write are atomic.
            let already = {
                let props = mutator.read().node_properties(node).ok_or_else(|| {
                    StoreError::invariant("pin write target has no properties".to_string())
                })?;
                matches!(props.get(&pinned_key), Some(Value::Bool(true)))
            };
            if already == set_to {
                PinWrite::Noop
            } else {
                let diff = PropertyDiff::new([(key(IS_PINNED)?, Value::Bool(set_to))], [])?;
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
                PinWrite::Applied
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
    //! `tests/pin_write.rs`.

    use aionforge_domain::blocks::{Identity, Stats};
    use aionforge_domain::ids::Id;
    use aionforge_domain::namespace::Namespace;
    use aionforge_domain::nodes::forensic::AuditKind;
    use aionforge_domain::nodes::semantic::{Fact, FactStatus};
    use aionforge_domain::time::Timestamp;
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
            actor_id: Id::from_content_hash(b"test-pinner"),
            payload: serde_json::json!({"reason": "test"}),
            signature: String::new(),
            occurred_at: ts("2026-06-06T12:00:00-05:00[America/Chicago]"),
        }
    }

    #[test]
    fn pin_then_unpin_restores_the_exact_property_map() {
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
            object: ObjectValue::Text("pin round-trip".to_string()),
            confidence: 0.9,
            status: FactStatus::Active,
            statement: "tests pin round-trip".to_string(),
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
        let pinned_key = db_string(IS_PINNED).unwrap();

        let pinned = store
            .set_pinned(
                node,
                &audit_event(AuditKind::Pin, fact.identity.id, b"rt-pin"),
            )
            .expect("pin");
        assert_eq!(pinned, PinWrite::Applied);
        let mid = props_of();
        assert_eq!(mid.get(&pinned_key), Some(&Value::Bool(true)));
        // Exactly one key differs in value and none in presence: status and expiry are
        // untouched, so the pin can never blur a lifecycle signature.
        let mid_without_pin = PropertyMap::from_pairs(
            mid.iter()
                .filter(|(name, _)| **name != pinned_key)
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect::<Vec<_>>(),
        )
        .expect("rebuild map");
        let original_without_pin = PropertyMap::from_pairs(
            original
                .iter()
                .filter(|(name, _)| **name != pinned_key)
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect::<Vec<_>>(),
        )
        .expect("rebuild map");
        assert_eq!(
            mid_without_pin, original_without_pin,
            "the flip touches is_pinned and nothing else"
        );

        let cleared = store
            .clear_pinned(
                node,
                &audit_event(AuditKind::Unpin, fact.identity.id, b"rt-unpin"),
            )
            .expect("unpin");
        assert_eq!(cleared, PinWrite::Applied);
        assert_eq!(
            props_of(),
            original,
            "unpin restores the byte-identical property map"
        );
    }
}
