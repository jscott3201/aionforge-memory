//! The signature-verification seam for Ed25519 provenance (06 §3).
//!
//! Writes carry an Ed25519 signature over a canonical [`signing`](crate::signing)
//! payload. The substrate stores the writer's public key (`Agent.public_key`) and the
//! signature (`ProvenanceRecord.signature`) and *verifies* — a private key never enters
//! the process. This module declares the verification seam and its typed error; the
//! Ed25519 implementation lives in the trust layer (M4), so this crate stays free of a
//! crypto dependency. The seam is generic over the message bytes, so the same primitive
//! verifies provenance now and attestation (M4.T04) later.

use thiserror::Error;

use crate::ids::Id;

/// Resolves a writer agent's stored base64 public key by agent id.
///
/// Implemented by the trust layer over the store, so the domain seam stays free of a
/// store dependency. Returns `Ok(None)` for an agent that is not registered — the gate
/// treats an unregistered writer as a failure (fail-closed) when signed writes are on.
pub trait PublicKeyResolver: Send + Sync {
    /// The base64 public key registered for `agent_id`, or `None` if no such agent.
    fn public_key(&self, agent_id: &Id) -> Result<Option<String>, ResolveError>;
}

/// A backend failure while resolving a public key. The underlying store error is carried
/// as text so this domain seam need not name the store's error type.
#[derive(Debug, Error)]
#[error("public-key resolution failed: {0}")]
pub struct ResolveError(pub String);

/// Verifies an Ed25519 signature over arbitrary message bytes against a public key.
///
/// The key and signature are the base64 strings stored on the `Agent` (`public_key`)
/// and the `ProvenanceRecord` (`signature`); the implementation decodes and checks them.
/// Implemented by the trust layer.
pub trait SignatureVerifier: Send + Sync {
    /// Verify `signature_b64` over `message` against `public_key_b64`. Returns `Ok(())`
    /// only on a valid signature; every other outcome is a [`VerifyError`].
    fn verify(
        &self,
        public_key_b64: &str,
        signature_b64: &str,
        message: &[u8],
    ) -> Result<(), VerifyError>;
}

/// Why a signature failed to verify.
#[derive(Debug, Error)]
pub enum VerifyError {
    /// The stored public key was not valid base64 or not a 32-byte Ed25519 key.
    #[error("malformed public key")]
    MalformedPublicKey,
    /// The signature was not valid base64 or not a 64-byte Ed25519 signature.
    #[error("malformed signature")]
    MalformedSignature,
    /// The signature did not verify against the key and message.
    #[error("signature does not verify")]
    Invalid,
}
