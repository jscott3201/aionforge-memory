//! Translation between a domain [`AuditEvent`] and a selene-db node (02 §4.11).
//!
//! A forensic kind carrying only the [`Identity`] block (no `Stats`). The `payload`
//! is an intentionally open `JSON` shape (02 §6.4), round-tripped as a native JSON
//! value; `kind` serializes to its `snake_case` spec string.

use aionforge_domain::blocks::Identity;
use aionforge_domain::ids::Id;
use aionforge_domain::nodes::forensic::AuditEvent;
use selene_core::{
    DbString, LabelDiff, LabelSet, NodeId, PropertyDiff, PropertyMap, Value, db_string,
};
use selene_graph::{Mutator, RowIndex, SeleneGraph};

use crate::convert::{
    as_id, as_namespace, as_str, as_timestamp, enum_from_value, enum_value, id_value,
    json_from_value, json_value, key, namespace_value, string_value, timestamp_value,
};
use crate::error::StoreError;

const ID: &str = "id";
const INGESTED_AT: &str = "ingested_at";
const NAMESPACE: &str = "namespace";
const EXPIRED_AT: &str = "expired_at";
const KIND: &str = "kind";
const SUBJECT_ID: &str = "subject_id";
const ACTOR_ID: &str = "actor_id";
const PAYLOAD: &str = "payload";
const SIGNATURE: &str = "signature";
const OCCURRED_AT: &str = "occurred_at";

/// The selene-db node label for an audit event.
pub(crate) fn label() -> Result<LabelSet, StoreError> {
    Ok(LabelSet::single(db_string(AuditEvent::LABEL)?))
}

/// Translate an [`AuditEvent`] into `(labels, properties)` for `create_node`.
pub(crate) fn to_node(event: &AuditEvent) -> Result<(LabelSet, PropertyMap), StoreError> {
    let mut pairs: Vec<(DbString, Value)> = Vec::with_capacity(10);

    pairs.push((key(ID)?, id_value(&event.identity.id)?));
    pairs.push((
        key(INGESTED_AT)?,
        timestamp_value(&event.identity.ingested_at),
    ));
    pairs.push((key(NAMESPACE)?, namespace_value(&event.identity.namespace)?));
    if let Some(expired_at) = &event.identity.expired_at {
        pairs.push((key(EXPIRED_AT)?, timestamp_value(expired_at)));
    }
    pairs.push((key(KIND)?, enum_value(&event.kind)?));
    pairs.push((key(SUBJECT_ID)?, id_value(&event.subject_id)?));
    pairs.push((key(ACTOR_ID)?, id_value(&event.actor_id)?));
    pairs.push((key(PAYLOAD)?, json_value(&event.payload)?));
    pairs.push((key(SIGNATURE)?, string_value(&event.signature)?));
    pairs.push((key(OCCURRED_AT)?, timestamp_value(&event.occurred_at)));

    Ok((label()?, PropertyMap::from_pairs(pairs)?))
}

/// Reconstruct an [`AuditEvent`] from a node's stored property map.
pub(crate) fn from_properties(props: &PropertyMap) -> Result<AuditEvent, StoreError> {
    let get =
        |name: &str| -> Result<Option<&Value>, StoreError> { Ok(props.get(&db_string(name)?)) };
    let require = |name: &str| -> Result<&Value, StoreError> {
        get(name)?.ok_or_else(|| StoreError::decode(format!("missing required property `{name}`")))
    };

    Ok(AuditEvent {
        identity: Identity {
            id: as_id(require(ID)?)?,
            ingested_at: as_timestamp(require(INGESTED_AT)?)?,
            namespace: as_namespace(require(NAMESPACE)?)?,
            expired_at: get(EXPIRED_AT)?.map(as_timestamp).transpose()?,
        },
        kind: enum_from_value(require(KIND)?)?,
        subject_id: as_id(require(SUBJECT_ID)?)?,
        actor_id: as_id(require(ACTOR_ID)?)?,
        payload: json_from_value(require(PAYLOAD)?)?,
        signature: as_str(require(SIGNATURE)?)?.to_string(),
        occurred_at: as_timestamp(require(OCCURRED_AT)?)?,
    })
}

/// Find an audit event already written with this content-addressed id, returning its node.
/// `AuditEvent.id` is `UNIQUE`, so this is a probe — the dedup that makes a replay of the same
/// episode write no second copy of an audit it already produced (04 §3), mirroring the fact
/// and note paths. Private on purpose: every author goes through [`ensure_event`], so nothing
/// outside this module can probe-and-create around the signature reconcile.
fn find_existing(snapshot: &SeleneGraph, id: &Id) -> Result<Option<NodeId>, StoreError> {
    let label = db_string(AuditEvent::LABEL)?;
    let prop = db_string(ID)?;
    let value = id_value(id)?;
    let Some(rows) = snapshot.nodes_with_property_eq(&label, &prop, &value) else {
        return Ok(None);
    };
    Ok(rows
        .iter()
        .find_map(|row| snapshot.node_id_for_row(RowIndex::new(row))))
}

/// Outcome of [`ensure_event`]: the audit node, plus whether this call created it — so the
/// call sites that wire side effects only for a fresh node (the `AUDIT` edge in
/// `record_reliability_update` and the promotion paths) can branch on `created`.
pub(crate) struct EnsuredAudit {
    pub(crate) node: NodeId,
    pub(crate) created: bool,
}

/// What [`ensure_event`] does to the stored row's `signature` when a content-identical
/// event (same content-addressed `AuditEvent.id`) is re-emitted.
#[derive(Debug, PartialEq, Eq)]
enum SignatureAction {
    /// Leave the stored signature untouched.
    Keep,
    /// Overwrite the stored signature with the incoming copy's.
    Upgrade,
}

/// Decide what happens to the stored signature when a content-identical event is re-emitted.
///
/// `AuditEvent.id` is content-addressed over everything EXCEPT the signature (a signature
/// cannot sign itself — `audit_payload` excludes it), and the id is UNIQUE. So two copies of
/// "the same" event can disagree only in their `signature` field, and exactly one row exists.
/// This function is the single policy point deciding which signature that row keeps.
///
/// The policy is a one-way latch: blank → signed is the only legal transition.
///
/// The four cases and why each lands where it does:
/// 1. `stored == incoming` (both blank or both the same bytes) — a pure replay (Ed25519 is
///    deterministic, so honest crash-replays re-sign identical bytes). Keep skips the write.
/// 2. `stored` blank, `incoming` non-blank — a signed re-emit reaches a row some earlier
///    unsigned write (or an attacker pre-placing a blank shadow copy) owns. UPGRADE — the
///    shadow-fix this funnel exists for.
/// 3. `stored` non-blank, `incoming` blank — an unsigned re-emit reaches an already-signed
///    row (signing later disabled, or a downgrade attempt). Keep: proof is monotone — a row
///    can gain a signature, never lose one.
/// 4. both non-blank but different — reachable honestly by a verbatim crash-heal AFTER a key
///    rotation (same content, re-signed by the new key). Keep: the verifier binds an event
///    to the key whose validity window contains the row's STORED `ingested_at`
///    (audit_verifier.rs cutover), and a dedup hit never re-stamps `ingested_at` — so the
///    stored signature is the one that verifies, and overwriting it with newer-key bytes
///    would flip the row from `Valid` to `Invalid`. `(ingested_at, signature)` must stay a
///    matched pair; Keep is the only action that never tears it apart.
fn signature_action(stored: &str, incoming: &str) -> SignatureAction {
    if stored.is_empty() && !incoming.is_empty() {
        SignatureAction::Upgrade
    } else {
        SignatureAction::Keep
    }
}

/// The stored `signature` property of an existing audit node.
fn stored_signature(snapshot: &SeleneGraph, node: NodeId) -> Result<String, StoreError> {
    let props = snapshot
        .node_properties(node)
        .ok_or_else(|| StoreError::decode("audit node has no properties".to_string()))?;
    let value = props
        .get(&db_string(SIGNATURE)?)
        .ok_or_else(|| StoreError::decode("audit node missing `signature`".to_string()))?;
    Ok(as_str(value)?.to_string())
}

/// Find-or-create the audit node for this content-addressed event, reconciling the stored
/// signature with the incoming copy's (M4.T06 PR-5e). Every audit author funnels through
/// here so the dedup probe and the signature policy stay one code path.
///
/// The probe runs against the in-txn working graph under the caller's write lock, so the
/// probe, any signature reconcile, and the create are atomic with the caller's other writes.
///
/// Why reconcile at all: before this, "row exists" meant "write nothing", so whichever copy
/// of an event landed FIRST owned the row forever — anyone able to write the store could
/// pre-place a blank-signature copy of a predictable content id, and the later, legitimately
/// signed emit deduped into a silent no-op. The blank copy permanently shadowed the signed
/// one and read back as benign "legacy unsigned".
pub(crate) fn ensure_event(
    mutator: &mut Mutator<'_, '_>,
    event: &AuditEvent,
) -> Result<EnsuredAudit, StoreError> {
    match find_existing(mutator.read(), &event.identity.id)? {
        Some(node) => {
            let stored = stored_signature(mutator.read(), node)?;
            if signature_action(&stored, &event.signature) == SignatureAction::Upgrade {
                mutator.update_node(
                    node,
                    LabelDiff::new([], [])?,
                    PropertyDiff::new([(key(SIGNATURE)?, string_value(&event.signature)?)], [])?,
                )?;
            } else if !stored.is_empty() && !event.signature.is_empty() && stored != event.signature
            {
                // Two signed copies disagree (policy case 4): either a verbatim crash-heal
                // re-signed by a post-rotation key (benign — the stored signature is the one
                // the verifier's key window matches) or an attempt to overwrite a valid
                // signature. Kept either way; rare enough to be worth a forensic trace.
                tracing::warn!(
                    audit = %event.identity.id,
                    "audit dedup kept the stored signature over a conflicting signed copy"
                );
            }
            Ok(EnsuredAudit {
                node,
                created: false,
            })
        }
        None => {
            let (labels, props) = to_node(event)?;
            Ok(EnsuredAudit {
                node: mutator.create_node(labels, props)?,
                created: true,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SignatureAction, signature_action};

    /// The one-way latch, pinned case by case: blank → signed is the ONLY transition that
    /// writes. Flipping any row of this table is a security regression (silent downgrade,
    /// or tearing the verifier's `(ingested_at, signature)` pairing), not a refactor.
    #[test]
    fn the_signature_latch_upgrades_only_blank_to_signed() {
        let cases = [
            ("", "", SignatureAction::Keep, "blank replay"),
            ("sig", "sig", SignatureAction::Keep, "signed replay"),
            ("", "sig", SignatureAction::Upgrade, "the shadow heal"),
            ("sig", "", SignatureAction::Keep, "downgrade refused"),
            (
                "sig",
                "other",
                SignatureAction::Keep,
                "conflict keeps stored",
            ),
        ];
        for (stored, incoming, expected, why) in cases {
            assert_eq!(signature_action(stored, incoming), expected, "{why}");
        }
    }
}
