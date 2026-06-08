//! Ed25519 verification of provenance signatures (06 §3).
//!
//! The trust layer is the only place that names the crypto crate. [`Ed25519Verifier`]
//! decodes the base64 public key and signature stored on the `Agent` / `ProvenanceRecord`
//! and checks the signature against a canonical
//! [`signing`](aionforge_domain::signing) payload, using strict verification so a
//! malleable signature is rejected. It only ever verifies — a private key never enters
//! the substrate.

use aionforge_domain::verify::{SignatureVerifier, VerifyError};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signature, VerifyingKey};

/// An Ed25519 [`SignatureVerifier`] over base64-encoded keys and signatures.
#[derive(Debug, Default, Clone, Copy)]
pub struct Ed25519Verifier;

impl SignatureVerifier for Ed25519Verifier {
    fn verify(
        &self,
        public_key_b64: &str,
        signature_b64: &str,
        message: &[u8],
    ) -> Result<(), VerifyError> {
        let key_bytes = BASE64
            .decode(public_key_b64)
            .map_err(|_| VerifyError::MalformedPublicKey)?;
        let key_array: [u8; 32] = key_bytes
            .as_slice()
            .try_into()
            .map_err(|_| VerifyError::MalformedPublicKey)?;
        let verifying_key =
            VerifyingKey::from_bytes(&key_array).map_err(|_| VerifyError::MalformedPublicKey)?;

        let sig_bytes = BASE64
            .decode(signature_b64)
            .map_err(|_| VerifyError::MalformedSignature)?;
        let sig_array: [u8; 64] = sig_bytes
            .as_slice()
            .try_into()
            .map_err(|_| VerifyError::MalformedSignature)?;
        let signature = Signature::from_bytes(&sig_array);

        verifying_key
            .verify_strict(message, &signature)
            .map_err(|_| VerifyError::Invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionforge_domain::ids::Id;
    use aionforge_domain::signing::provenance_payload;
    use aionforge_domain::time::Timestamp;
    use ed25519_dalek::{Signer, SigningKey};
    use jiff::Timestamp as Instant;
    use jiff::tz::TimeZone;

    fn ts(ms: i64) -> Timestamp {
        Instant::from_millisecond(ms)
            .unwrap()
            .to_zoned(TimeZone::UTC)
    }

    fn id(seed: u128) -> Id {
        Id::from_uuid(uuid::Uuid::from_u128(seed))
    }

    /// A deterministic keypair from a fixed seed — no RNG, so the fixture is stable across
    /// runs and the `rand_core` feature stays out of the dependency tree.
    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn public_key_b64(key: &SigningKey) -> String {
        BASE64.encode(key.verifying_key().to_bytes())
    }

    fn sign_b64(key: &SigningKey, message: &[u8]) -> String {
        BASE64.encode(key.sign(message).to_bytes())
    }

    #[test]
    fn accepts_a_valid_signature() {
        let key = signing_key(7);
        let payload = provenance_payload(&id(1), &id(2), &ts(1_700_000_000_000));
        let signature = sign_b64(&key, &payload);
        assert!(
            Ed25519Verifier
                .verify(&public_key_b64(&key), &signature, &payload)
                .is_ok()
        );
    }

    #[test]
    fn rejects_a_tampered_payload() {
        let key = signing_key(7);
        let payload = provenance_payload(&id(1), &id(2), &ts(10));
        let signature = sign_b64(&key, &payload);
        let tampered = provenance_payload(&id(1), &id(2), &ts(11));
        assert!(matches!(
            Ed25519Verifier.verify(&public_key_b64(&key), &signature, &tampered),
            Err(VerifyError::Invalid)
        ));
    }

    #[test]
    fn rejects_a_flipped_signature_byte() {
        let key = signing_key(7);
        let payload = provenance_payload(&id(1), &id(2), &ts(10));
        let mut raw = key.sign(&payload).to_bytes();
        raw[0] ^= 0x01;
        let signature = BASE64.encode(raw);
        assert!(matches!(
            Ed25519Verifier.verify(&public_key_b64(&key), &signature, &payload),
            Err(VerifyError::Invalid)
        ));
    }

    #[test]
    fn rejects_a_different_key() {
        let signer = signing_key(7);
        let other = signing_key(9);
        let payload = provenance_payload(&id(1), &id(2), &ts(10));
        let signature = sign_b64(&signer, &payload);
        assert!(matches!(
            Ed25519Verifier.verify(&public_key_b64(&other), &signature, &payload),
            Err(VerifyError::Invalid)
        ));
    }

    #[test]
    fn rejects_a_malformed_public_key() {
        let payload = provenance_payload(&id(1), &id(2), &ts(10));
        assert!(matches!(
            Ed25519Verifier.verify("not valid base64 !!!", "AAAA", &payload),
            Err(VerifyError::MalformedPublicKey)
        ));
    }

    #[test]
    fn rejects_a_wrong_length_key() {
        let key = signing_key(7);
        let payload = provenance_payload(&id(1), &id(2), &ts(10));
        let signature = sign_b64(&key, &payload);
        // Valid base64, but 16 bytes — not a 32-byte Ed25519 key.
        let short_key = BASE64.encode([0u8; 16]);
        assert!(matches!(
            Ed25519Verifier.verify(&short_key, &signature, &payload),
            Err(VerifyError::MalformedPublicKey)
        ));
    }

    #[test]
    fn rejects_a_wrong_length_signature() {
        let key = signing_key(7);
        let payload = provenance_payload(&id(1), &id(2), &ts(10));
        // Valid base64 key, valid base64 signature, but 10 bytes — not 64.
        let short_sig = BASE64.encode([0u8; 10]);
        assert!(matches!(
            Ed25519Verifier.verify(&public_key_b64(&key), &short_sig, &payload),
            Err(VerifyError::MalformedSignature)
        ));
    }
}
