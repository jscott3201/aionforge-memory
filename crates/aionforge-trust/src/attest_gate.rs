//! The Ed25519 signed-attestation gate (06 §4).
//!
//! [`AttestationGate`] is the attestation twin of
//! [`SignedWriteGate`](crate::gate::SignedWriteGate): it composes the same seams — the
//! [`Ed25519Verifier`] over a canonical [`attestation_payload`] and a
//! [`PublicKeyResolver`] over `Agent.public_key` — with a [`WallClock`] and a clock-skew
//! tolerance. It admits an attestation only when the attester is registered, the signature
//! verifies, and the timestamp sits inside the skew window. It only ever verifies — a
//! private key never enters the substrate.
//!
//! The error taxonomy is deliberately coarse so the gate is neither an enrollment oracle
//! ("is this agent registered?") nor a forge oracle ("which check failed?"): an unknown
//! attester and a bad signature both surface as one rejection, while a clock-skew rejection
//! is reported on its own so an honest client can resync. A backend read failure is an
//! availability fault, not a security rejection.

use std::sync::Arc;

use aionforge_domain::gate::WallClock;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::signing::{attestation_payload, core_edit_attestation_payload};
use aionforge_domain::time::Timestamp;
use aionforge_domain::verify::{PublicKeyResolver, SignatureVerifier};

use crate::signing::Ed25519Verifier;

/// Why a signed attestation was refused.
#[derive(Debug)]
pub enum AttestError {
    /// The attestation failed a security check (the cause is for the audit, not the caller).
    Rejected(AttestRejection),
    /// A backend read failed while resolving the attester's key — an availability fault, not an
    /// attack. Carries the underlying message.
    Backend(String),
}

/// The specific reason a signed attestation was rejected. The caller sees a coarse error; this
/// is what the audit records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestRejection {
    /// No registered agent owns the attester id (fail-closed; never a lazy enrollment).
    UnknownAttester,
    /// The signature did not verify against the attester's registered key.
    BadSignature,
    /// The attestation timestamp deviates from the substrate clock beyond the tolerance.
    ClockSkew {
        /// The absolute deviation, in milliseconds.
        skew_ms: i64,
        /// The configured tolerance, in milliseconds.
        tolerance_ms: u64,
    },
}

impl From<AttestRejection> for AttestError {
    fn from(rejection: AttestRejection) -> Self {
        AttestError::Rejected(rejection)
    }
}

/// The Ed25519 signed-attestation gate (06 §4).
pub struct AttestationGate {
    verifier: Ed25519Verifier,
    resolver: Arc<dyn PublicKeyResolver>,
    clock: Arc<dyn WallClock>,
    tolerance_ms: u64,
}

// Manual `Debug` that does not recurse into the resolver/clock seams — neither carries a
// `Debug` bound, and a key resolver should not print its store in a debug dump.
impl std::fmt::Debug for AttestationGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttestationGate")
            .field("tolerance_ms", &self.tolerance_ms)
            .finish_non_exhaustive()
    }
}

impl AttestationGate {
    /// Compose a gate over a verifier, a public-key resolver, a wall clock, and the skew
    /// tolerance in milliseconds.
    #[must_use]
    pub fn new(
        verifier: Ed25519Verifier,
        resolver: Arc<dyn PublicKeyResolver>,
        clock: Arc<dyn WallClock>,
        tolerance_ms: u64,
    ) -> Self {
        Self {
            verifier,
            resolver,
            clock,
            tolerance_ms,
        }
    }

    /// Verify a signed attestation of `fact_id` by `attester_id` at `attested_at`.
    ///
    /// Skew first (a replayed/storming attestation is dropped before any store read), then
    /// fail-closed key resolution, then signature verification over the canonical payload
    /// recomputed from the request — the gate never trusts client-sent payload bytes.
    ///
    /// # Errors
    /// [`AttestError::Rejected`] for a skew/unknown-attester/bad-signature failure;
    /// [`AttestError::Backend`] if the key resolution read itself failed.
    pub fn admit(
        &self,
        fact_id: &Id,
        attester_id: &Id,
        attested_at: &Timestamp,
        signature_b64: &str,
    ) -> Result<(), AttestError> {
        let public_key = self.resolve_in_window(attester_id, attested_at)?;
        let payload = attestation_payload(fact_id, attester_id, attested_at);
        self.verifier
            .verify(&public_key, signature_b64, &payload)
            .map_err(|_| AttestRejection::BadSignature.into())
    }

    /// Verify a signed core-block edit attestation: the same skew, enrollment, and
    /// signature discipline as [`AttestationGate::admit`], but over the
    /// transition-binding [`core_edit_attestation_payload`] (05 §4, M5.T04).
    ///
    /// A core block's **stable** id cannot bind a vote to content the way a fact's
    /// content-addressed id does, so the core-edit vote signs the exact prior-to-new
    /// content transition — a voucher collected for one proposed edit can never
    /// validate a different replacement of the same block, even inside the skew
    /// window.
    ///
    /// # Errors
    /// [`AttestError::Rejected`] for a skew/unknown-attester/bad-signature failure;
    /// [`AttestError::Backend`] if the key resolution read itself failed.
    pub fn admit_core_edit(
        &self,
        block_id: &Id,
        attester_id: &Id,
        prior_content_hash: &ContentHash,
        new_content_hash: &ContentHash,
        attested_at: &Timestamp,
        signature_b64: &str,
    ) -> Result<(), AttestError> {
        let public_key = self.resolve_in_window(attester_id, attested_at)?;
        let payload = core_edit_attestation_payload(
            block_id,
            attester_id,
            prior_content_hash,
            new_content_hash,
            attested_at,
        );
        self.verifier
            .verify(&public_key, signature_b64, &payload)
            .map_err(|_| AttestRejection::BadSignature.into())
    }

    /// The shared front half of every admit: skew first (a replayed/storming
    /// attestation is dropped before any store read), then fail-closed key resolution.
    fn resolve_in_window(
        &self,
        attester_id: &Id,
        attested_at: &Timestamp,
    ) -> Result<String, AttestError> {
        let now_ms = self.clock.now().timestamp().as_millisecond();
        let attest_ms = attested_at.timestamp().as_millisecond();
        let skew_ms = (now_ms - attest_ms).abs();
        if skew_ms > i64::try_from(self.tolerance_ms).unwrap_or(i64::MAX) {
            return Err(AttestRejection::ClockSkew {
                skew_ms,
                tolerance_ms: self.tolerance_ms,
            }
            .into());
        }
        match self.resolver.public_key(attester_id) {
            Ok(Some(key)) => Ok(key),
            Ok(None) => Err(AttestRejection::UnknownAttester.into()),
            Err(error) => Err(AttestError::Backend(error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionforge_domain::verify::ResolveError;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;
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

    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn public_key_b64(key: &SigningKey) -> String {
        BASE64.encode(key.verifying_key().to_bytes())
    }

    fn sign_b64(key: &SigningKey, message: &[u8]) -> String {
        BASE64.encode(key.sign(message).to_bytes())
    }

    struct FixedClock(Timestamp);
    impl WallClock for FixedClock {
        fn now(&self) -> Timestamp {
            self.0.clone()
        }
    }

    struct OneKeyResolver {
        agent: Id,
        public_key: String,
    }
    impl PublicKeyResolver for OneKeyResolver {
        fn public_key(&self, agent_id: &Id) -> Result<Option<String>, ResolveError> {
            Ok((agent_id == &self.agent).then(|| self.public_key.clone()))
        }
    }

    struct FailingResolver;
    impl PublicKeyResolver for FailingResolver {
        fn public_key(&self, _agent_id: &Id) -> Result<Option<String>, ResolveError> {
            Err(ResolveError("backend down".to_string()))
        }
    }

    fn gate(
        resolver: Arc<dyn PublicKeyResolver>,
        now_ms: i64,
        tolerance_ms: u64,
    ) -> AttestationGate {
        AttestationGate::new(
            Ed25519Verifier,
            resolver,
            Arc::new(FixedClock(ts(now_ms))),
            tolerance_ms,
        )
    }

    #[test]
    fn admits_a_valid_in_window_attestation() {
        let key = signing_key(7);
        let (fact, attester, at) = (id(1), id(2), ts(1_000));
        let signature = sign_b64(&key, &attestation_payload(&fact, &attester, &at));
        let resolver = Arc::new(OneKeyResolver {
            agent: attester,
            public_key: public_key_b64(&key),
        });
        let gate = gate(resolver, 1_000, 60_000);
        assert!(gate.admit(&fact, &attester, &at, &signature).is_ok());
    }

    #[test]
    fn rejects_an_unregistered_attester_fail_closed() {
        let key = signing_key(7);
        let (fact, attester, at) = (id(1), id(2), ts(1_000));
        let signature = sign_b64(&key, &attestation_payload(&fact, &attester, &at));
        let resolver = Arc::new(OneKeyResolver {
            agent: id(99),
            public_key: public_key_b64(&key),
        });
        let gate = gate(resolver, 1_000, 60_000);
        assert!(matches!(
            gate.admit(&fact, &attester, &at, &signature),
            Err(AttestError::Rejected(AttestRejection::UnknownAttester))
        ));
    }

    #[test]
    fn rejects_a_foreign_key_signature() {
        let signer = signing_key(7);
        let enrolled = signing_key(9);
        let (fact, attester, at) = (id(1), id(2), ts(1_000));
        let signature = sign_b64(&signer, &attestation_payload(&fact, &attester, &at));
        let resolver = Arc::new(OneKeyResolver {
            agent: attester,
            public_key: public_key_b64(&enrolled),
        });
        let gate = gate(resolver, 1_000, 60_000);
        assert!(matches!(
            gate.admit(&fact, &attester, &at, &signature),
            Err(AttestError::Rejected(AttestRejection::BadSignature))
        ));
    }

    #[test]
    fn rejects_an_attestation_for_a_different_fact() {
        let key = signing_key(7);
        let (fact, attester, at) = (id(1), id(2), ts(1_000));
        // Sign over a different fact id than the one presented.
        let signature = sign_b64(&key, &attestation_payload(&id(42), &attester, &at));
        let resolver = Arc::new(OneKeyResolver {
            agent: attester,
            public_key: public_key_b64(&key),
        });
        let gate = gate(resolver, 1_000, 60_000);
        assert!(matches!(
            gate.admit(&fact, &attester, &at, &signature),
            Err(AttestError::Rejected(AttestRejection::BadSignature))
        ));
    }

    #[test]
    fn admits_at_the_skew_boundary_and_rejects_just_past_it() {
        let key = signing_key(7);
        let (fact, attester, at) = (id(1), id(2), ts(1_000));
        let signature = sign_b64(&key, &attestation_payload(&fact, &attester, &at));
        let make = |now_ms: i64| {
            let resolver = Arc::new(OneKeyResolver {
                agent: attester,
                public_key: public_key_b64(&key),
            });
            gate(resolver, now_ms, 5_000)
        };
        assert!(make(6_000).admit(&fact, &attester, &at, &signature).is_ok());
        assert!(matches!(
            make(6_001).admit(&fact, &attester, &at, &signature),
            Err(AttestError::Rejected(AttestRejection::ClockSkew {
                skew_ms: 5_001,
                tolerance_ms: 5_000
            }))
        ));
        // Symmetric: a future-dated attestation past the bound is rejected too.
        assert!(matches!(
            make(-4_001).admit(&fact, &attester, &at, &signature),
            Err(AttestError::Rejected(AttestRejection::ClockSkew { .. }))
        ));
    }

    #[test]
    fn admits_a_core_edit_vote_for_the_exact_transition_only() {
        let key = signing_key(7);
        let (block, attester, at) = (id(1), id(2), ts(1_000));
        let prior = ContentHash::of(b"who I was");
        let new = ContentHash::of(b"who I am becoming");
        let signature = sign_b64(
            &key,
            &core_edit_attestation_payload(&block, &attester, &prior, &new, &at),
        );
        let resolver = Arc::new(OneKeyResolver {
            agent: attester,
            public_key: public_key_b64(&key),
        });
        let gate = gate(resolver, 1_000, 60_000);

        // The vouched transition verifies.
        assert!(
            gate.admit_core_edit(&block, &attester, &prior, &new, &at, &signature)
                .is_ok()
        );
        // The same vote presented for a *different* replacement is a forged voucher —
        // this is the content-swap surface the transition binding closes.
        let swapped = ContentHash::of(b"something the attester never saw");
        assert!(matches!(
            gate.admit_core_edit(&block, &attester, &prior, &swapped, &at, &signature),
            Err(AttestError::Rejected(AttestRejection::BadSignature))
        ));
        // And a fact-shaped signature over the bare ids does not pass the core-edit gate.
        let bare = sign_b64(&key, &attestation_payload(&block, &attester, &at));
        assert!(matches!(
            gate.admit_core_edit(&block, &attester, &prior, &new, &at, &bare),
            Err(AttestError::Rejected(AttestRejection::BadSignature))
        ));
    }

    #[test]
    fn a_backend_fault_is_an_availability_error_not_a_rejection() {
        let key = signing_key(7);
        let (fact, attester, at) = (id(1), id(2), ts(1_000));
        let signature = sign_b64(&key, &attestation_payload(&fact, &attester, &at));
        let gate = gate(Arc::new(FailingResolver), 1_000, 60_000);
        assert!(matches!(
            gate.admit(&fact, &attester, &at, &signature),
            Err(AttestError::Backend(_))
        ));
    }
}
