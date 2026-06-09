//! The substrate's audit-signing key (06 §6): the one author-channel carve-out.
//!
//! On the writer channel the substrate only ever verifies (06 §3) — a host's private
//! key never enters the process. The audit channel is different: the substrate is
//! itself the author of the audit events it emits, so it holds its own Ed25519 keypair
//! and signs through the [`AuditEventSigner`] seam. [`AuditSigner`] is that
//! implementation, and [`SecretSeed`] is the 32-byte secret it is built from — wiped
//! on drop, redacted in `Debug`.
//!
//! [`AuditSigner::mint`] holds the workspace's only direct RNG-API call: key
//! generation is confined to it, enforced in CI by `check-audit-keygen-confined.sh`,
//! because feature unification makes dalek's keygen code reachable from every crate
//! that links it. The gate denies RNG identifiers, so its guarantee is exactly that —
//! no key generation and no direct RNG calls outside this file. Indirect entropy it
//! cannot see (UUIDv7 id minting draws OS randomness through `uuid`) is outside its
//! scope. Where the seed lives between runs (env variable, custody file) is the
//! custody layer's concern — this module never does I/O.

use std::fmt;

use aionforge_domain::nodes::forensic::AuditEvent;
use aionforge_domain::signing::audit_payload;
use aionforge_domain::verify::AuditEventSigner;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signer, SigningKey};
use thiserror::Error;
use zeroize::Zeroizing;

/// A raw 32-byte Ed25519 audit-signing seed.
///
/// The buffer is zeroed when the value drops and `Debug` prints a redaction marker,
/// so the seed cannot leak through logs or error chains. Custody code reads the raw
/// bytes through [`SecretSeed::as_bytes`] solely to persist them; nothing else should
/// hold a copy. (`[u8; 32]` is `Copy`, so a constructor argument may leave a transient
/// copy on the caller's stack — callers that load a seed from storage should keep
/// their own buffer in a [`Zeroizing`] wrapper too.)
pub struct SecretSeed(Zeroizing<[u8; 32]>);

impl SecretSeed {
    /// Wrap raw seed bytes (the custody file stores exactly these 32 bytes).
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Decode a base64 seed (the form an environment variable carries).
    ///
    /// The decode lands in a caller-owned wiped buffer, so even a failed decode's
    /// partial output is zeroized — `Engine::decode` would build its own un-wiped
    /// `Vec`, and a mid-string error (say a stray `\r\n`) can write most of a real
    /// seed before it trips. The input is taken verbatim — no whitespace trimming —
    /// so a malformed value fails closed rather than being quietly repaired.
    ///
    /// # Errors
    /// [`KeyError::SeedNotBase64`] when the string does not decode;
    /// [`KeyError::SeedWrongLength`] when it decodes to anything but 32 bytes (for
    /// inputs too long to be a seed, the reported length is base64's estimate).
    pub fn from_base64(seed_b64: &str) -> Result<Self, KeyError> {
        // 33 bytes is the decode estimate for the 44 chars a 32-byte seed occupies;
        // a longer input overflows the slice and is rejected as wrong-length below.
        let mut decoded = Zeroizing::new([0u8; 33]);
        let len = BASE64
            .decode_slice(seed_b64, decoded.as_mut())
            .map_err(|err| match err {
                base64::DecodeSliceError::DecodeError(_) => KeyError::SeedNotBase64,
                base64::DecodeSliceError::OutputSliceTooSmall => {
                    KeyError::SeedWrongLength(base64::decoded_len_estimate(seed_b64.len()))
                }
            })?;
        let bytes: [u8; 32] = decoded[..len]
            .try_into()
            .map_err(|_| KeyError::SeedWrongLength(len))?;
        Ok(Self::from_bytes(bytes))
    }

    /// The raw seed bytes, for custody persistence only.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for SecretSeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretSeed(<redacted>)")
    }
}

/// Why an audit-signing key could not be built from the supplied material.
#[derive(Debug, Error)]
pub enum KeyError {
    /// The seed string was not valid base64.
    #[error("audit seed is not valid base64")]
    SeedNotBase64,
    /// The seed decoded, but not to the 32 bytes an Ed25519 seed must be. The length
    /// is safe to report; the content never is.
    #[error("audit seed must decode to exactly 32 bytes, got {0}")]
    SeedWrongLength(usize),
}

/// The substrate's Ed25519 audit signer: signs the canonical
/// [`audit_payload`] bytes of each substrate-authored [`AuditEvent`].
///
/// Built either by [`AuditSigner::mint`] (a fresh key from the OS CSPRNG, first
/// enable) or [`AuditSigner::from_seed`] (every later start, from custody). Signing is
/// deterministic (RFC 8032): rebuilding the same event re-signs to the same base64
/// string, which is what keeps the store's dedup-by-id write a true no-op for
/// content-addressed audit families. The signer never exposes its secret — the seed
/// is surfaced exactly once, by `mint`, for custody to persist.
pub struct AuditSigner {
    key: SigningKey,
}

impl AuditSigner {
    /// Mint a fresh audit key from the operating system's CSPRNG.
    ///
    /// The workspace's only direct RNG-API call (CI-enforced as key-generation
    /// confinement). Returns the seed alongside the signer so custody can persist
    /// it; after this call the signer never gives the secret back.
    #[must_use]
    pub fn mint() -> (Self, SecretSeed) {
        let key = SigningKey::generate(&mut rand_core::OsRng);
        let seed = SecretSeed::from_bytes(key.to_bytes());
        (Self { key }, seed)
    }

    /// Rebuild the signer from a persisted seed. Infallible: every 32-byte string is
    /// a valid Ed25519 seed, so a seed that loaded is a key that works.
    #[must_use]
    pub fn from_seed(seed: &SecretSeed) -> Self {
        Self {
            key: SigningKey::from_bytes(seed.as_bytes()),
        }
    }

    /// The base64 of the 32-byte verifying key — the form the keyring file pins and a
    /// `KeyRotation` payload announces.
    #[must_use]
    pub fn public_key_b64(&self) -> String {
        BASE64.encode(self.key.verifying_key().to_bytes())
    }
}

impl AuditEventSigner for AuditSigner {
    fn sign(&self, event: &AuditEvent) -> String {
        BASE64.encode(self.key.sign(&audit_payload(event)).to_bytes())
    }
}

impl fmt::Debug for AuditSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuditSigner")
            .field("public_key", &self.public_key_b64())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signing::Ed25519Verifier;
    use aionforge_domain::blocks::Identity;
    use aionforge_domain::ids::Id;
    use aionforge_domain::namespace::Namespace;
    use aionforge_domain::nodes::forensic::AuditKind;
    use aionforge_domain::time::Timestamp;
    use aionforge_domain::verify::SignatureVerifier;

    fn event() -> AuditEvent {
        let at: Timestamp = "2026-06-09T09:00:00-05:00[America/Chicago]"
            .parse()
            .expect("valid zoned datetime");
        AuditEvent {
            identity: Identity {
                id: Id::from_content_hash(b"audit-signer-test"),
                ingested_at: at.clone(),
                namespace: Namespace::System,
                expired_at: None,
            },
            kind: AuditKind::Promote,
            subject_id: Id::from_content_hash(b"subject"),
            actor_id: Id::from_content_hash(b"actor"),
            payload: serde_json::json!({"reason": "test"}),
            signature: String::new(),
            occurred_at: at,
        }
    }

    fn seed(byte: u8) -> SecretSeed {
        SecretSeed::from_bytes([byte; 32])
    }

    /// The full closure of the channel: what `AuditSigner` signs, `Ed25519Verifier`
    /// accepts — over the same canonical payload bytes, against the announced key.
    #[test]
    fn a_signed_event_verifies_against_the_announced_public_key() {
        let signer = AuditSigner::from_seed(&seed(7));
        let signature = signer.sign(&event());
        assert!(
            Ed25519Verifier
                .verify(
                    &signer.public_key_b64(),
                    &signature,
                    &audit_payload(&event())
                )
                .is_ok()
        );
    }

    /// Any change to a signed field breaks verification — here the bound instant.
    #[test]
    fn a_tampered_event_fails_verification() {
        let signer = AuditSigner::from_seed(&seed(7));
        let signature = signer.sign(&event());
        let mut tampered = event();
        tampered.occurred_at = "2026-06-09T09:00:01-05:00[America/Chicago]"
            .parse()
            .expect("valid zoned datetime");
        assert!(
            Ed25519Verifier
                .verify(
                    &signer.public_key_b64(),
                    &signature,
                    &audit_payload(&tampered)
                )
                .is_err()
        );
    }

    /// The [`AuditEventSigner`] determinism contract: a crash-replay that rebuilds the
    /// signer from the same seed re-signs the same event to the identical string, so
    /// the store's dedup-by-id write stays a no-op.
    #[test]
    fn signing_is_deterministic_across_signer_rebuilds() {
        let first = AuditSigner::from_seed(&seed(7)).sign(&event());
        let second = AuditSigner::from_seed(&seed(7)).sign(&event());
        assert_eq!(first, second);
    }

    /// The custody round-trip `mint` exists for: the seed handed out at mint time
    /// rebuilds a signer with the same key.
    #[test]
    fn a_minted_seed_reconstructs_the_same_signer() {
        let (minted, minted_seed) = AuditSigner::mint();
        let rebuilt = AuditSigner::from_seed(&minted_seed);
        assert_eq!(minted.public_key_b64(), rebuilt.public_key_b64());
        assert_eq!(minted.sign(&event()), rebuilt.sign(&event()));
    }

    #[test]
    fn two_mints_yield_distinct_keys() {
        let (first, _) = AuditSigner::mint();
        let (second, _) = AuditSigner::mint();
        assert_ne!(first.public_key_b64(), second.public_key_b64());
    }

    #[test]
    fn a_seed_round_trips_through_base64() {
        let encoded = BASE64.encode([9u8; 32]);
        let decoded = SecretSeed::from_base64(&encoded).expect("a 32-byte base64 seed parses");
        assert_eq!(decoded.as_bytes(), &[9u8; 32]);
    }

    #[test]
    fn a_malformed_seed_is_rejected() {
        assert!(matches!(
            SecretSeed::from_base64("not valid base64 !!!"),
            Err(KeyError::SeedNotBase64)
        ));
        let short = BASE64.encode([0u8; 16]);
        assert!(matches!(
            SecretSeed::from_base64(&short),
            Err(KeyError::SeedWrongLength(16))
        ));
        // Whitespace is not repaired: fail-closed over forgiving. `\r\n` is the case
        // where the decoder writes most of a real seed before tripping — which is why
        // from_base64 decodes into its own wiped buffer.
        for tail in ["\n", "\r\n", " "] {
            let padded = format!("{}{tail}", BASE64.encode([7u8; 32]));
            assert!(SecretSeed::from_base64(&padded).is_err());
        }
        // An input too long to be a seed is refused as wrong-length.
        let long = BASE64.encode([0u8; 64]);
        assert!(matches!(
            SecretSeed::from_base64(&long),
            Err(KeyError::SeedWrongLength(_))
        ));
    }

    /// The secret never reaches a log line: `Debug` is a fixed marker for the seed,
    /// and the signer prints only its public half.
    #[test]
    fn debug_output_redacts_the_secret() {
        assert_eq!(format!("{:?}", seed(7)), "SecretSeed(<redacted>)");
        let signer = AuditSigner::from_seed(&seed(7));
        let rendered = format!("{signer:?}");
        assert!(rendered.contains(&signer.public_key_b64()));
        assert!(!rendered.contains("key: SigningKey"));
    }
}
