//! The keyring-anchored audit verifier (06 §6): per-row signature status, never a
//! query-level error.
//!
//! [`AuditVerifier`] is built from a loaded [`AuditKeyring`] — the out-of-band sole
//! trust anchor. A missing or broken keyring file fails at load, before a verifier
//! exists; nothing here ever falls back to trusting material found in the store.
//!
//! The must-sign cutover is keyed on **`identity.ingested_at`** — the substrate's
//! write-clock — against the keyring's creation instant. `occurred_at` is
//! attacker-writable content (a forger can backdate it); `ingested_at` is excluded
//! from the signed payload and is only ever re-stamped *forward* on recovery, so
//! keying on it fails strict rather than open. The residual of that strictness is
//! documented on [`AuditVerifier::status`]: a recovery that re-stamps rows past a
//! later rotation boundary reads pre-rotation signatures as [`AuditStatus::Invalid`]
//! — visible and investigable, never silently trusted.
//!
//! An ordinary event verifies against the key whose validity window contains its
//! `ingested_at`, so a leaked *retired* seed cannot mint events that read as valid
//! today. `KeyRotation` events are the one documented exception: a rotation is
//! signed by the *predecessor* but lands in the store inside the *successor's*
//! window, so the verifier resolves its signer from the payload and requires the
//! whole announcement to sit chain-adjacent in the keyring: for genesis the
//! announced key IS the keyring's genesis, and for a rotation the named predecessor
//! retires exactly on the announced admission AND the announced key is that
//! predecessor's actual successor in the chain. Both halves matter — without the
//! announced-key pin, a leaked retired seed could sign a "rotation" announcing any
//! key it liked, dated inside its own dead window. A rotation announcing a key the
//! keyring never admitted verifies as [`AuditStatus::Untrusted`] no matter who
//! signed it.

use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind, KeyRotationPayload};
use aionforge_domain::signing::audit_payload;
use aionforge_domain::verify::SignatureVerifier;

use crate::audit_keyring::AuditKeyring;
use crate::signing::Ed25519Verifier;

/// The per-row verification status of one audit event. A status is descriptive,
/// never an error: readers surface it beside the row and counting queries must be
/// provably invariant to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditStatus {
    /// The signature verifies against the trusted key for the event's window.
    Valid,
    /// Blank signature from before the must-sign cutover — the legacy shape every
    /// event had before signing was enabled. Benign.
    Unsigned,
    /// Blank signature *after* the cutover: every substrate author signs from the
    /// cutover on, so a blank here means the signature was stripped or the row was
    /// forged into the store. Hard fail.
    Downgraded,
    /// A signature is present but does not verify against the window's trusted key
    /// — tampered content, a stripped-and-replaced payload, or a signer outside its
    /// validity window (including a leaked retired seed minting fresh rows).
    Invalid,
    /// No trusted key answers for this event: signed before the keyring existed,
    /// inside no admitted window, or a `KeyRotation` whose announced chain position
    /// the keyring never admitted.
    Untrusted,
}

/// Verifies audit-event signatures against the keyring's admitted keys and their
/// validity windows.
#[derive(Debug, Clone)]
pub struct AuditVerifier {
    keyring: AuditKeyring,
}

impl AuditVerifier {
    /// Anchor a verifier on a loaded keyring. The keyring file is the sole
    /// authority; load (and its fail-closed behavior on a missing or broken file)
    /// happens before this, in [`AuditKeyring::load`].
    #[must_use]
    pub fn from_keyring(keyring: AuditKeyring) -> Self {
        Self { keyring }
    }

    /// The verification status of one event — total, infallible, per-row.
    ///
    /// Strictness residual: `ingested_at` may be re-stamped forward on recovery; a
    /// row carried past a later rotation boundary then verifies against the wrong
    /// (newer) key and reads [`AuditStatus::Invalid`]. That bias is deliberate —
    /// the alternative cutover key (`occurred_at`) is attacker-writable.
    #[must_use]
    pub fn status(&self, event: &AuditEvent) -> AuditStatus {
        let ingested = event.identity.ingested_at.timestamp();
        if event.signature.is_empty() {
            return if ingested < self.keyring.created_at().timestamp() {
                AuditStatus::Unsigned
            } else {
                AuditStatus::Downgraded
            };
        }

        let signer_pubkey = if matches!(event.kind, AuditKind::KeyRotation) {
            self.rotation_signer(event)
        } else {
            self.keyring
                .key_for(&event.identity.ingested_at)
                .map(|key| key.pubkey_b64.as_str())
        };
        let Some(pubkey) = signer_pubkey else {
            return AuditStatus::Untrusted;
        };
        match Ed25519Verifier.verify(pubkey, &event.signature, &audit_payload(event)) {
            Ok(()) => AuditStatus::Valid,
            Err(_) => AuditStatus::Invalid,
        }
    }

    /// The trusted signer of a `KeyRotation` event, resolved from its payload and
    /// pinned to the keyring's chain. Returns the *keyring's* copy of the key, so
    /// verification can never run against a string the event supplied for itself.
    ///
    /// `None` (→ [`AuditStatus::Untrusted`]) when the payload does not parse — with
    /// no named signer there is nothing sound to verify against, even though a
    /// parse failure alone is not proof of tampering — or when the announced chain
    /// position is not the keyring's.
    fn rotation_signer(&self, event: &AuditEvent) -> Option<&str> {
        let payload = KeyRotationPayload::from_value(&event.payload).ok()?;
        match payload.predecessor_pubkey_b64 {
            // Genesis is self-signed: trusted only if the announced key IS the
            // keyring's genesis key.
            None => {
                let genesis = self.keyring.keys().first()?;
                (payload.announced_pubkey_b64 == genesis.pubkey_b64)
                    .then_some(genesis.pubkey_b64.as_str())
            }
            // A rotation is signed by its predecessor: trusted only if that key is
            // admitted, retires exactly on the announced admission instant, AND the
            // announced key is its actual successor. The successor pin is load-
            // bearing: the predecessor's retirement instant is public (it sits in
            // the keyring), so a leaked retired seed could otherwise sign a
            // "rotation" announcing an attacker key dated on that instant.
            Some(predecessor) => {
                let keys = self.keyring.keys();
                let position = keys.iter().position(|key| {
                    key.pubkey_b64 == predecessor
                        && key.retired_at.as_ref().is_some_and(|retired| {
                            retired.timestamp() == payload.admitted_at.timestamp()
                        })
                })?;
                let successor = keys.get(position + 1)?;
                (successor.pubkey_b64 == payload.announced_pubkey_b64)
                    .then_some(keys[position].pubkey_b64.as_str())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit_rotation::{genesis_rotation, rotate_key};
    use crate::audit_signer::{AuditSigner, SecretSeed};
    use crate::system_audit::{content_id, system_identity};
    use aionforge_domain::ids::Id;
    use aionforge_domain::time::Timestamp;
    use aionforge_domain::verify::AuditEventSigner;

    fn signer(byte: u8) -> AuditSigner {
        AuditSigner::from_seed(&SecretSeed::from_bytes([byte; 32]))
    }

    fn at(text: &str) -> Timestamp {
        text.parse().expect("valid zoned datetime")
    }

    const GENESIS: &str = "2026-06-09T14:00:00+00:00[UTC]";
    const ROTATION: &str = "2026-07-01T00:00:00+00:00[UTC]";

    /// An ordinary (non-rotation) event ingested and occurring at `ingested`.
    fn event_at(ingested: &str) -> AuditEvent {
        let now = at(ingested);
        AuditEvent {
            identity: system_identity(content_id("test", ingested), &now),
            kind: AuditKind::Promote,
            subject_id: Id::from_content_hash(b"subject"),
            actor_id: Id::from_content_hash(b"actor"),
            payload: serde_json::json!({"reason": "test"}),
            signature: String::new(),
            occurred_at: now,
        }
    }

    fn signed_event_at(ingested: &str, key: &AuditSigner) -> AuditEvent {
        let mut event = event_at(ingested);
        event.signature = key.sign(&event);
        event
    }

    fn verifier() -> AuditVerifier {
        let (_, keyring) = genesis_rotation(&signer(7), &at(GENESIS));
        AuditVerifier::from_keyring(keyring)
    }

    /// A two-key chain: key 7 retired on the rotation instant, key 9 active.
    fn rotated_verifier() -> AuditVerifier {
        let (_, keyring) = genesis_rotation(&signer(7), &at(GENESIS));
        let (_, extended) =
            rotate_key(&keyring, &signer(7), &signer(9), &at(ROTATION)).expect("rotates");
        AuditVerifier::from_keyring(extended)
    }

    #[test]
    fn the_cutover_is_keyed_on_ingested_at_not_occurred_at() {
        // Pre-cutover blank rows are the benign legacy shape.
        let legacy = event_at("2026-06-01T00:00:00+00:00[UTC]");
        assert_eq!(verifier().status(&legacy), AuditStatus::Unsigned);

        // A blank row ingested after the cutover is a hard fail even when its
        // attacker-writable occurred_at is backdated to look legacy.
        let mut backdated = event_at("2026-06-10T00:00:00+00:00[UTC]");
        backdated.occurred_at = at("2026-06-01T00:00:00+00:00[UTC]");
        assert_eq!(verifier().status(&backdated), AuditStatus::Downgraded);

        // The boundary instant itself: genesis signs from created_at on, so the
        // must-sign regime is inclusive — a blank row AT the cutover hard-fails,
        // and one a tick earlier stays legacy-benign. A `<` -> `<=` flip on the
        // cutover comparison must fail here.
        assert_eq!(
            verifier().status(&event_at(GENESIS)),
            AuditStatus::Downgraded
        );
        let just_before = event_at("2026-06-09T13:59:59+00:00[UTC]");
        assert_eq!(verifier().status(&just_before), AuditStatus::Unsigned);
    }

    #[test]
    fn a_signed_event_in_window_is_valid_and_tampering_invalidates_it() {
        let event = signed_event_at("2026-06-10T00:00:00+00:00[UTC]", &signer(7));
        assert_eq!(verifier().status(&event), AuditStatus::Valid);

        let mut tampered = event;
        tampered.payload = serde_json::json!({"reason": "rewritten"});
        assert_eq!(verifier().status(&tampered), AuditStatus::Invalid);
    }

    #[test]
    fn window_binding_rejects_a_retired_seed_signing_fresh_rows() {
        let verifier = rotated_verifier();
        // The retired key 7 verifies inside its own window...
        let in_window = signed_event_at("2026-06-15T00:00:00+00:00[UTC]", &signer(7));
        assert_eq!(verifier.status(&in_window), AuditStatus::Valid);
        // ...but a leaked key 7 minting a row AFTER its retirement reads Invalid.
        let leaked = signed_event_at("2026-07-02T00:00:00+00:00[UTC]", &signer(7));
        assert_eq!(verifier.status(&leaked), AuditStatus::Invalid);
        // The active key 9 verifies in the new window.
        let current = signed_event_at("2026-07-02T00:00:00+00:00[UTC]", &signer(9));
        assert_eq!(verifier.status(&current), AuditStatus::Valid);
    }

    #[test]
    fn a_signature_before_any_admitted_window_is_untrusted() {
        let prehistoric = signed_event_at("2026-06-01T00:00:00+00:00[UTC]", &signer(7));
        assert_eq!(verifier().status(&prehistoric), AuditStatus::Untrusted);
    }

    #[test]
    fn rotation_events_verify_through_their_chain_position() {
        let (genesis_event, keyring) = genesis_rotation(&signer(7), &at(GENESIS));
        let (rotation_event, extended) =
            rotate_key(&keyring, &signer(7), &signer(9), &at(ROTATION)).expect("rotates");
        let verifier = AuditVerifier::from_keyring(extended);

        // Genesis: self-signed by the keyring's genesis key.
        assert_eq!(verifier.status(&genesis_event), AuditStatus::Valid);
        // Rotation: signed by the predecessor although it lands in the successor's
        // window — the documented special case.
        assert_eq!(verifier.status(&rotation_event), AuditStatus::Valid);
    }

    #[test]
    fn a_forged_rotation_announcing_an_unadmitted_key_is_untrusted() {
        // An attacker mints their own "rotation" enrolling their key, signed by
        // themselves. The keyring never admitted it, so it enrolls nothing.
        let attacker = signer(13);
        let (forged, _) = genesis_rotation(&attacker, &at("2026-07-05T00:00:00+00:00[UTC]"));
        assert_eq!(verifier().status(&forged), AuditStatus::Untrusted);

        // Same for a fake successor rotation naming a predecessor the keyring has
        // never retired on that instant.
        let (_, fake_ring) = genesis_rotation(&signer(7), &at(GENESIS));
        let (fake_rotation, _) = rotate_key(
            &fake_ring,
            &signer(7),
            &attacker,
            &at("2026-07-05T00:00:00+00:00[UTC]"),
        )
        .expect("builds");
        // Verified against the REAL keyring (which has no such retirement).
        assert_eq!(verifier().status(&fake_rotation), AuditStatus::Untrusted);
    }

    /// The sharpest forgery: a LEAKED RETIRED seed forging a rotation dated exactly
    /// on its own (public) retirement instant. The predecessor pin alone would pass
    /// it — only the announced-must-be-the-actual-successor pin stops it.
    #[test]
    fn a_leaked_retired_seed_cannot_forge_a_valid_rotation_at_its_own_retirement() {
        use aionforge_domain::nodes::forensic::KeyRotationPayload;
        use aionforge_domain::time::to_utc;

        let verifier = rotated_verifier(); // key 7 retired at ROTATION, key 9 active
        let attacker = signer(13);
        let boundary = to_utc(&at(ROTATION));
        let payload = KeyRotationPayload {
            announced_pubkey_b64: attacker.public_key_b64(),
            predecessor_pubkey_b64: Some(signer(7).public_key_b64()),
            admitted_at: boundary.clone(),
            retired_at: Some(boundary.clone()),
        };
        let mut forged = AuditEvent {
            identity: system_identity(
                content_id("key_rotation", &attacker.public_key_b64()),
                &boundary,
            ),
            kind: AuditKind::KeyRotation,
            subject_id: Id::from_content_hash(b"forged-subject"),
            actor_id: Id::from_content_hash(b"forged-actor"),
            payload: payload.to_value(),
            signature: String::new(),
            occurred_at: boundary,
        };
        forged.signature = signer(7).sign(&forged); // genuinely signed by leaked key 7
        assert_eq!(verifier.status(&forged), AuditStatus::Untrusted);

        // The genuine rotation through the same boundary still verifies.
        let (_, keyring) = genesis_rotation(&signer(7), &at(GENESIS));
        let (genuine, _) =
            rotate_key(&keyring, &signer(7), &signer(9), &at(ROTATION)).expect("rotates");
        assert_eq!(verifier.status(&genuine), AuditStatus::Valid);
    }

    #[test]
    fn a_rotation_payload_that_does_not_parse_is_untrusted() {
        let mut event = event_at("2026-06-10T00:00:00+00:00[UTC]");
        event.kind = AuditKind::KeyRotation;
        event.payload = serde_json::json!({"not": "a rotation"});
        event.signature = signer(7).sign(&event);
        assert_eq!(verifier().status(&event), AuditStatus::Untrusted);
    }
}
