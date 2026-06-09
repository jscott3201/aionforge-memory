//! Builders for the signed `KeyRotation` audit events (06 §6): the in-store echo of
//! the keyring file.
//!
//! Two shapes, both pure — no I/O here; the caller sequences the writes:
//!
//! - **Genesis** ([`genesis_rotation`]): the substrate's first key announces itself,
//!   self-signed (there is no earlier key to vouch). The caller saves the returned
//!   keyring *first*, then commits the event: a crash in between is healed by
//!   re-emitting from the same still-held seed **with the saved keyring's
//!   `created_at` as `now`** — the builder is deterministic over `(seed, now)` and
//!   the content-addressed id carries no time component, so a fresh clock would
//!   mint the *same id over different bytes*; re-supplying the saved instant
//!   rebuilds byte-identical canonical bytes, re-signs to the same signature
//!   (Ed25519 is deterministic), and dedups as a true no-op.
//! - **Rotation** ([`rotate_key`]): a successor key is announced by — and the event
//!   is signed by — the *outgoing* key, which is the "a new key is admitted only by
//!   the holder of an admitted key" rule made concrete. v1 exposes no rotation
//!   trigger; the engine auto-mints genesis only. When a trigger lands, its
//!   orchestration must make the **incoming seed durable first** (staged beside,
//!   never over, the outgoing seed file), then commit the signed event, then
//!   promote the staged seed to the live file, then save the keyring. The outgoing
//!   seed survives until the commit lands — the event can only be re-emitted while
//!   it exists — and the staged incoming seed is what keeps a crash between commit
//!   and seed-promotion from stranding a durable announcement whose key existed
//!   only in process memory and can now never enter service.
//!
//! The id is `content_id("key_rotation", announced_pubkey)`: a public key enters
//! service exactly once (the keyring refuses re-admission), so one-per-key-lifetime
//! is the correct dedup grain. Every stored timestamp is stamped in UTC so a
//! reader's tz database never gets a vote on whether the payload parses.

use aionforge_domain::ids::Id;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind, KeyRotationPayload};
use aionforge_domain::time::{Timestamp, to_utc};
use aionforge_domain::verify::AuditEventSigner;

use crate::audit_keyring::{AuditKeyring, KeyringError};
use crate::audit_signer::AuditSigner;
use crate::system_audit::{content_id, system_identity};

/// The stable node identity of a signing key in the audit graph: subject of the
/// rotation that announces it, actor of the rotation it signs off.
fn key_node_id(pubkey_b64: &str) -> Id {
    Id::from_content_hash(format!("audit_key|{pubkey_b64}").as_bytes())
}

fn rotation_event(
    payload: &KeyRotationPayload,
    actor_pubkey_b64: &str,
    at: &Timestamp,
) -> AuditEvent {
    AuditEvent {
        identity: system_identity(
            content_id("key_rotation", &payload.announced_pubkey_b64),
            at,
        ),
        kind: AuditKind::KeyRotation,
        subject_id: key_node_id(&payload.announced_pubkey_b64),
        actor_id: key_node_id(actor_pubkey_b64),
        payload: payload.to_value(),
        signature: String::new(),
        occurred_at: at.clone(),
    }
}

/// The genesis announcement: a fresh keyring anchored on `signer`'s key, plus the
/// self-signed `KeyRotation` event that echoes it into the audit trail.
///
/// `now` is the host instant (never an ambient clock read); it becomes the keyring's
/// `created_at` — the must-sign cutover — and the key's admission, both in UTC.
#[must_use]
pub fn genesis_rotation(signer: &AuditSigner, now: &Timestamp) -> (AuditEvent, AuditKeyring) {
    let at = to_utc(now);
    let pubkey = signer.public_key_b64();
    let keyring = AuditKeyring::genesis(pubkey.clone(), &at)
        .expect("a signer's own public key is base64 for 32 bytes");
    let payload = KeyRotationPayload {
        announced_pubkey_b64: pubkey.clone(),
        predecessor_pubkey_b64: None,
        admitted_at: at.clone(),
        retired_at: None,
    };
    let mut event = rotation_event(&payload, &pubkey, &at);
    event.signature = signer.sign(&event);
    (event, keyring)
}

/// A successor announcement: the keyring extended with `incoming`'s key, plus the
/// `KeyRotation` event signed by the *outgoing* key.
///
/// # Errors
/// [`KeyringError::BrokenChain`] when `outgoing` does not hold the keyring's active
/// key (only an admitted key admits a successor), when the incoming key is already
/// in the chain, or when `now` does not move time forward.
pub fn rotate_key(
    keyring: &AuditKeyring,
    outgoing: &AuditSigner,
    incoming: &AuditSigner,
    now: &Timestamp,
) -> Result<(AuditEvent, AuditKeyring), KeyringError> {
    let at = to_utc(now);
    let outgoing_pubkey = outgoing.public_key_b64();
    if keyring.active().map(|key| key.pubkey_b64.as_str()) != Some(outgoing_pubkey.as_str()) {
        return Err(KeyringError::BrokenChain(
            "only the holder of the active key may admit a successor",
        ));
    }
    let incoming_pubkey = incoming.public_key_b64();
    let mut extended = keyring.clone();
    extended.rotate(incoming_pubkey.clone(), &at)?;

    let payload = KeyRotationPayload {
        announced_pubkey_b64: incoming_pubkey,
        predecessor_pubkey_b64: Some(outgoing_pubkey.clone()),
        admitted_at: at.clone(),
        retired_at: Some(at.clone()),
    };
    let mut event = rotation_event(&payload, &outgoing_pubkey, &at);
    event.signature = outgoing.sign(&event);
    Ok((event, extended))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit_signer::SecretSeed;
    use aionforge_domain::signing::audit_payload;

    fn signer(byte: u8) -> AuditSigner {
        AuditSigner::from_seed(&SecretSeed::from_bytes([byte; 32]))
    }

    fn at(text: &str) -> Timestamp {
        text.parse().expect("valid zoned datetime")
    }

    #[test]
    fn a_genesis_re_emission_is_byte_identical() {
        let now = at("2026-06-09T09:00:00-05:00[America/Chicago]");
        let (event, keyring) = genesis_rotation(&signer(7), &now);
        let (again, _) = genesis_rotation(&signer(7), &now);
        assert_eq!(event.identity.id, again.identity.id, "content-addressed id");
        assert_eq!(audit_payload(&event), audit_payload(&again));
        assert_eq!(event.signature, again.signature, "deterministic re-sign");
        assert_eq!(
            keyring.created_at().timestamp(),
            event.occurred_at.timestamp(),
            "the cutover is the genesis instant"
        );
        assert_eq!(
            keyring.active().map(|k| k.pubkey_b64.clone()),
            Some(signer(7).public_key_b64())
        );
    }

    #[test]
    fn genesis_stamps_utc_into_the_payload() {
        let now = at("2026-06-09T09:00:00-05:00[America/Chicago]");
        let (event, _) = genesis_rotation(&signer(7), &now);
        let parsed = KeyRotationPayload::from_value(&event.payload).expect("parses");
        let wire = serde_json::to_string(&parsed.admitted_at).expect("serializes");
        assert!(wire.contains("+00:00[UTC]"), "UTC on the wire, got {wire}");
        assert!(parsed.predecessor_pubkey_b64.is_none());
        assert!(parsed.retired_at.is_none());
    }

    #[test]
    fn a_rotation_is_signed_by_the_outgoing_key_and_extends_the_chain() {
        let genesis_at = at("2026-06-09T14:00:00+00:00[UTC]");
        let (_, keyring) = genesis_rotation(&signer(7), &genesis_at);
        let later = at("2026-07-01T00:00:00+00:00[UTC]");
        let (event, extended) =
            rotate_key(&keyring, &signer(7), &signer(9), &later).expect("rotates");

        let payload = KeyRotationPayload::from_value(&event.payload).expect("parses");
        assert_eq!(payload.announced_pubkey_b64, signer(9).public_key_b64());
        assert_eq!(
            payload.predecessor_pubkey_b64.as_deref(),
            Some(signer(7).public_key_b64().as_str())
        );
        assert_eq!(
            payload.retired_at.as_ref().map(jiff::Zoned::timestamp),
            Some(payload.admitted_at.timestamp()),
            "the predecessor retires on the successor's admission"
        );
        assert_eq!(extended.keys().len(), 2);
        assert_eq!(event.actor_id, key_node_id(&signer(7).public_key_b64()));
        assert_eq!(event.subject_id, key_node_id(&signer(9).public_key_b64()));
        // The original keyring is untouched — the caller swaps to the extended one.
        assert_eq!(keyring.keys().len(), 1);
    }

    #[test]
    fn only_the_active_key_holder_may_rotate() {
        let (_, keyring) = genesis_rotation(&signer(7), &at("2026-06-09T14:00:00+00:00[UTC]"));
        let err = rotate_key(
            &keyring,
            &signer(5),
            &signer(9),
            &at("2026-07-01T00:00:00+00:00[UTC]"),
        )
        .expect_err("a non-admitted key admits nothing");
        assert!(matches!(err, KeyringError::BrokenChain(_)));
    }
}
