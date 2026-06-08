//! The Ed25519 signed-write gate (06 §3).
//!
//! [`SignedWriteGate`] is the concrete [`ProvenanceGate`]: it composes the PR-A seams —
//! the [`Ed25519Verifier`] over a canonical [`provenance_payload`] and the
//! [`StoreKeyResolver`](crate::StoreKeyResolver) over `Agent.public_key` — with a
//! [`WallClock`] and a clock-skew tolerance. It admits a write only when the writer is
//! registered, the signature verifies, and the timestamp sits inside the skew window. It
//! only ever verifies — a private key never enters the substrate.

use aionforge_domain::gate::{GateError, GateRejection, ProvenanceGate, WallClock};
use aionforge_domain::ids::Id;
use aionforge_domain::signing::provenance_payload;
use aionforge_domain::time::Timestamp;
use aionforge_domain::verify::{PublicKeyResolver, SignatureVerifier};

use crate::signing::Ed25519Verifier;

/// The production wall clock: the system zoned time. The read is transient — used to
/// accept or reject a write, never stored — so the no-ambient-clock-for-stored-time rule
/// holds.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemWallClock;

impl WallClock for SystemWallClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// The Ed25519 signed-write gate (06 §3).
///
/// Holds the verifier, the public-key resolver, the wall clock, and the skew tolerance in
/// milliseconds. The resolver and clock are trait objects so the engine injects the store
/// and the system clock in production and a test injects a fixed clock and a fixture key.
pub struct SignedWriteGate {
    verifier: Ed25519Verifier,
    resolver: std::sync::Arc<dyn PublicKeyResolver>,
    clock: std::sync::Arc<dyn WallClock>,
    tolerance_ms: u64,
}

// Manual `Debug` (the trait requires it) that does not recurse into the resolver/clock
// seams — neither carries a `Debug` bound, and a key resolver should not print its store
// in a debug dump anyway.
impl std::fmt::Debug for SignedWriteGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignedWriteGate")
            .field("tolerance_ms", &self.tolerance_ms)
            .finish_non_exhaustive()
    }
}

impl SignedWriteGate {
    /// Compose a gate over a verifier, a public-key resolver, a wall clock, and the skew
    /// tolerance in milliseconds.
    #[must_use]
    pub fn new(
        verifier: Ed25519Verifier,
        resolver: std::sync::Arc<dyn PublicKeyResolver>,
        clock: std::sync::Arc<dyn WallClock>,
        tolerance_ms: u64,
    ) -> Self {
        Self {
            verifier,
            resolver,
            clock,
            tolerance_ms,
        }
    }
}

impl ProvenanceGate for SignedWriteGate {
    fn admit(
        &self,
        subject_id: &Id,
        writer_agent_id: &Id,
        ingested_at: &Timestamp,
        signature_b64: &str,
    ) -> Result<(), GateError> {
        // Skew first: drop a replayed or storming write cheaply, before any store read.
        // The window is symmetric, so a future-dated write is rejected exactly like a
        // stale one.
        let now_ms = self.clock.now().timestamp().as_millisecond();
        let write_ms = ingested_at.timestamp().as_millisecond();
        let skew_ms = (now_ms - write_ms).abs();
        if skew_ms > i64::try_from(self.tolerance_ms).unwrap_or(i64::MAX) {
            return Err(GateRejection::ClockSkew {
                skew_ms,
                tolerance_ms: self.tolerance_ms,
            }
            .into());
        }

        // Resolve the writer's registered key. Unknown ⇒ a fail-closed rejection (never a
        // lazy enrollment); a backend read failure is an availability fault, not an attack.
        let public_key = match self.resolver.public_key(writer_agent_id) {
            Ok(Some(key)) => key,
            Ok(None) => return Err(GateRejection::UnknownWriter.into()),
            Err(error) => return Err(GateError::Backend(error.to_string())),
        };

        // Verify the signature over the canonical payload recomputed from the request
        // fields — the gate never trusts client-sent payload bytes.
        let payload = provenance_payload(subject_id, writer_agent_id, ingested_at);
        self.verifier
            .verify(&public_key, signature_b64, &payload)
            .map_err(|_| GateRejection::BadSignature.into())
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
    use std::sync::Arc;

    fn ts(ms: i64) -> Timestamp {
        Instant::from_millisecond(ms)
            .unwrap()
            .to_zoned(TimeZone::UTC)
    }

    fn id(seed: u128) -> Id {
        Id::from_uuid(uuid::Uuid::from_u128(seed))
    }

    /// A deterministic keypair from a fixed seed — no RNG, so the fixture is stable and the
    /// `rand_core` feature stays out of the dependency tree.
    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn public_key_b64(key: &SigningKey) -> String {
        BASE64.encode(key.verifying_key().to_bytes())
    }

    fn sign_b64(key: &SigningKey, message: &[u8]) -> String {
        BASE64.encode(key.sign(message).to_bytes())
    }

    /// A clock pinned to a fixed instant, so the skew window is deterministic.
    struct FixedClock(Timestamp);
    impl WallClock for FixedClock {
        fn now(&self) -> Timestamp {
            self.0.clone()
        }
    }

    /// A resolver that returns one registered key, or `None` for any other agent.
    struct OneKeyResolver {
        agent: Id,
        public_key: String,
    }
    impl PublicKeyResolver for OneKeyResolver {
        fn public_key(&self, agent_id: &Id) -> Result<Option<String>, ResolveError> {
            Ok((agent_id == &self.agent).then(|| self.public_key.clone()))
        }
    }

    /// A resolver whose backend always fails, to exercise the availability path.
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
    ) -> SignedWriteGate {
        SignedWriteGate::new(
            Ed25519Verifier,
            resolver,
            Arc::new(FixedClock(ts(now_ms))),
            tolerance_ms,
        )
    }

    #[test]
    fn admits_a_valid_in_window_signature() {
        let key = signing_key(7);
        let (subject, writer, at) = (id(1), id(2), ts(1_000));
        let payload = provenance_payload(&subject, &writer, &at);
        let signature = sign_b64(&key, &payload);
        let resolver = Arc::new(OneKeyResolver {
            agent: writer,
            public_key: public_key_b64(&key),
        });
        // now == write time, so zero skew.
        let gate = gate(resolver, 1_000, 60_000);
        assert!(gate.admit(&subject, &writer, &at, &signature).is_ok());
    }

    #[test]
    fn rejects_an_unregistered_writer_fail_closed() {
        let key = signing_key(7);
        let (subject, writer, at) = (id(1), id(2), ts(1_000));
        let signature = sign_b64(&key, &provenance_payload(&subject, &writer, &at));
        // Resolver knows a *different* agent, so the writer is unknown.
        let resolver = Arc::new(OneKeyResolver {
            agent: id(99),
            public_key: public_key_b64(&key),
        });
        let gate = gate(resolver, 1_000, 60_000);
        assert!(matches!(
            gate.admit(&subject, &writer, &at, &signature),
            Err(GateError::Rejected(GateRejection::UnknownWriter))
        ));
    }

    #[test]
    fn rejects_a_foreign_key_signature() {
        let signer = signing_key(7);
        let enrolled = signing_key(9);
        let (subject, writer, at) = (id(1), id(2), ts(1_000));
        let signature = sign_b64(&signer, &provenance_payload(&subject, &writer, &at));
        // The agent is enrolled with `enrolled`'s public key, but `signer` signed.
        let resolver = Arc::new(OneKeyResolver {
            agent: writer,
            public_key: public_key_b64(&enrolled),
        });
        let gate = gate(resolver, 1_000, 60_000);
        assert!(matches!(
            gate.admit(&subject, &writer, &at, &signature),
            Err(GateError::Rejected(GateRejection::BadSignature))
        ));
    }

    #[test]
    fn rejects_a_wrong_message_signature() {
        let key = signing_key(7);
        let (subject, writer, at) = (id(1), id(2), ts(1_000));
        // Sign over a *different* subject id than the one presented.
        let signature = sign_b64(&key, &provenance_payload(&id(42), &writer, &at));
        let resolver = Arc::new(OneKeyResolver {
            agent: writer,
            public_key: public_key_b64(&key),
        });
        let gate = gate(resolver, 1_000, 60_000);
        assert!(matches!(
            gate.admit(&subject, &writer, &at, &signature),
            Err(GateError::Rejected(GateRejection::BadSignature))
        ));
    }

    #[test]
    fn admits_at_the_skew_boundary_and_rejects_just_past_it() {
        let key = signing_key(7);
        let (subject, writer, at) = (id(1), id(2), ts(1_000));
        let signature = sign_b64(&key, &provenance_payload(&subject, &writer, &at));
        let make = |now_ms: i64| {
            let resolver = Arc::new(OneKeyResolver {
                agent: writer,
                public_key: public_key_b64(&key),
            });
            gate(resolver, now_ms, 5_000)
        };
        // Exactly at the bound (5s late) is admitted.
        assert!(
            make(6_000)
                .admit(&subject, &writer, &at, &signature)
                .is_ok()
        );
        // One millisecond past the bound is rejected, and the deviation is reported.
        assert!(matches!(
            make(6_001).admit(&subject, &writer, &at, &signature),
            Err(GateError::Rejected(GateRejection::ClockSkew {
                skew_ms: 5_001,
                tolerance_ms: 5_000
            }))
        ));
        // The window is symmetric: a future-dated write past the bound is rejected too.
        // The write instant is fixed at 1_000, so a clock at -4_001 makes the write
        // 5_001ms in the substrate's future — one past the 5_000ms bound.
        assert!(matches!(
            make(-4_001).admit(&subject, &writer, &at, &signature),
            Err(GateError::Rejected(GateRejection::ClockSkew { .. }))
        ));
    }

    #[test]
    fn a_stale_replay_is_rejected_by_skew() {
        let key = signing_key(7);
        let (subject, writer, at) = (id(1), id(2), ts(1_000));
        let signature = sign_b64(&key, &provenance_payload(&subject, &writer, &at));
        let resolver = Arc::new(OneKeyResolver {
            agent: writer,
            public_key: public_key_b64(&key),
        });
        // The clock has advanced far past the signed instant: a replay of the old envelope
        // is rejected even though its signature is still valid.
        let gate = gate(resolver, 1_000 + 600_000, 60_000);
        assert!(matches!(
            gate.admit(&subject, &writer, &at, &signature),
            Err(GateError::Rejected(GateRejection::ClockSkew { .. }))
        ));
    }

    #[test]
    fn a_backend_fault_is_an_availability_error_not_a_rejection() {
        let key = signing_key(7);
        let (subject, writer, at) = (id(1), id(2), ts(1_000));
        let signature = sign_b64(&key, &provenance_payload(&subject, &writer, &at));
        let gate = gate(Arc::new(FailingResolver), 1_000, 60_000);
        assert!(matches!(
            gate.admit(&subject, &writer, &at, &signature),
            Err(GateError::Backend(_))
        ));
    }

    #[test]
    fn rejects_a_malformed_signature_as_bad_signature() {
        let key = signing_key(7);
        let (subject, writer, at) = (id(1), id(2), ts(1_000));
        let resolver = Arc::new(OneKeyResolver {
            agent: writer,
            public_key: public_key_b64(&key),
        });
        let gate = gate(resolver, 1_000, 60_000);
        // A 10-byte "signature" is not a 64-byte Ed25519 signature — the verifier's
        // MalformedSignature collapses to BadSignature at the gate.
        let short = BASE64.encode([0u8; 10]);
        assert!(matches!(
            gate.admit(&subject, &writer, &at, &short),
            Err(GateError::Rejected(GateRejection::BadSignature))
        ));
    }
}
