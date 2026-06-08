//! The signed-write admission seam (06 §3).
//!
//! A signed-write deployment admits a capture only when its provenance signature
//! verifies against the writer's registered key and its timestamp sits inside the
//! substrate's clock-skew window. This module declares the admission seam
//! ([`ProvenanceGate`]), its typed failure ([`GateError`] / [`GateRejection`]), and the
//! wall-clock seam ([`WallClock`]) the skew check reads. All three are crypto-free, so
//! the capture crate can hold an `Option<Arc<dyn ProvenanceGate>>` without taking a
//! crypto dependency; the Ed25519 implementation lives in the trust layer, which
//! composes the [`SignatureVerifier`](crate::verify::SignatureVerifier) and
//! [`PublicKeyResolver`](crate::verify::PublicKeyResolver) seams with a clock.

use thiserror::Error;

use crate::ids::Id;
use crate::time::Timestamp;

/// Admits or rejects a signed write before any memory is shaped (06 §3).
///
/// The capture path holds an `Option<Arc<dyn ProvenanceGate>>`: `None` is the unsigned
/// fast path — no crypto, byte-identical to an unsigned deployment — and `Some` gates
/// every write. [`admit`](ProvenanceGate::admit) verifies the writer's Ed25519 signature
/// over the canonical [`provenance_payload`](crate::signing::provenance_payload) and
/// rejects a timestamp outside the configured skew window. It only ever verifies — a
/// private key never reaches the substrate.
///
/// The `Debug` bound matches the [`Authorizer`](crate::authz::Authorizer) seam, so the
/// capture path can hold one in a `Debug`-deriving struct.
pub trait ProvenanceGate: Send + Sync + std::fmt::Debug {
    /// Admit a write signed by `writer_agent_id` over the canonical
    /// `(subject_id, writer_agent_id, ingested_at)` payload, or fail.
    ///
    /// `signature_b64` is the writer's base64 Ed25519 signature. A [`GateError::Rejected`]
    /// is a fail-closed security decision (the caller writes no memory and records a
    /// rejection audit); a [`GateError::Backend`] is an availability fault resolving the
    /// key (not an attack — no security audit).
    fn admit(
        &self,
        subject_id: &Id,
        writer_agent_id: &Id,
        ingested_at: &Timestamp,
        signature_b64: &str,
    ) -> Result<(), GateError>;
}

/// Why an [`admit`](ProvenanceGate::admit) call failed: a security rejection or a backend
/// fault. They are kept distinct so a transient store outage is never recorded as an
/// attack, which would poison the audit log and the rejection metrics.
#[derive(Debug, Error)]
pub enum GateError {
    /// A security check rejected the write. Fail-closed: write no memory, record a
    /// rejection audit.
    #[error(transparent)]
    Rejected(#[from] GateRejection),
    /// A backend read failed while resolving the writer's key (availability, not an
    /// attack). Surfaced as an error with no security audit.
    #[error("the provenance gate could not resolve the writer's key: {0}")]
    Backend(String),
}

/// Why a signed write was rejected by a security check.
///
/// The capture path collapses [`UnknownWriter`](GateRejection::UnknownWriter) and
/// [`BadSignature`](GateRejection::BadSignature) into one client-facing error so the
/// substrate is neither an enrollment oracle ("is this agent registered?") nor a forge
/// oracle ("which check failed?"); the distinct cause is recorded in the audit, never
/// returned. A skew rejection is reported on its own so a client can resync its clock.
#[derive(Debug, Error)]
pub enum GateRejection {
    /// No public key is registered for the writer. Fail-closed: enrollment is explicit,
    /// never lazy, so an attacker cannot self-enroll a key to forge a write.
    #[error("the writer agent is not registered")]
    UnknownWriter,
    /// The signature did not verify against the writer's key and the canonical payload.
    #[error("the provenance signature did not verify")]
    BadSignature,
    /// The write's timestamp deviates from the substrate clock beyond the configured
    /// bound (replay/storm mitigation, 06 §3).
    #[error(
        "the write timestamp is {skew_ms}ms off the substrate clock, beyond the {tolerance_ms}ms bound"
    )]
    ClockSkew {
        /// The absolute deviation between the write timestamp and the substrate clock.
        skew_ms: i64,
        /// The configured tolerance the deviation exceeded.
        tolerance_ms: u64,
    },
}

/// A source of the current instant for the signed-write skew check.
///
/// The wall read is transient — used only to accept or reject, never stored — so the
/// no-ambient-clock-for-stored-time rule holds. It is injected, like the consolidator's
/// clock, so tests pin the substrate's notion of "now" and the skew window is
/// deterministic.
pub trait WallClock: Send + Sync {
    /// The current instant.
    fn now(&self) -> Timestamp;
}
