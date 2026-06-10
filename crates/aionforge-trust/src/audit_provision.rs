//! Startup provisioning for substrate audit signing (M4.T06 PR-5g2): one call that
//! resolves seed custody, anchors (or re-anchors to) the keyring, and hands the engine
//! everything it needs to flip `sign_audit_events` live.
//!
//! ## The crash protocols, implemented
//!
//! The 5d emitters fixed two protocols this module realizes:
//!
//! - **Genesis** — save the keyring FIRST, then commit the self-signed `KeyRotation`
//!   event. A crash between the two leaves a keyring without its in-store echo, which the
//!   next startup heals (below). The reverse order would leave an in-store event with no
//!   out-of-band anchor — exactly the forgeable in-band trust this design refuses.
//! - **Heal** — when the keyring already exists, the genesis event is REBUILT from the
//!   held seed with the keyring's saved `created_at` as `now` (the event id is
//!   content-addressed with no time component, so a fresh clock would mint the same id
//!   over different bytes). The caller commits it unconditionally: the audit write
//!   funnel's dedup makes it a no-op when the row is already present and a heal when the
//!   genesis crash window hit. Zero store-awareness lives here.
//!
//! A loaded keyring is cross-checked against the resolved seed by rebuilding genesis and
//! comparing — a seed that does not match the anchored key is a hard failure (the wrong
//! identity must never sign), never a silent re-anchor.

use std::path::Path;

use aionforge_domain::nodes::forensic::AuditEvent;
use aionforge_domain::time::Timestamp;
use secrecy::SecretString;

use crate::audit_custody::{CustodyError, ensure_audit_dir, resolve_audit_signer};
use crate::audit_keyring::{AuditKeyring, KeyringError, keyring_path};
use crate::audit_rotation::genesis_rotation;
use crate::audit_signer::AuditSigner;
use crate::audit_verifier::AuditVerifier;

/// Everything the engine needs to enable audit signing.
pub struct AuditProvision {
    /// The substrate's signer, to install on the store's commit-time stamp.
    pub signer: AuditSigner,
    /// The keyring-anchored verifier, to install on the read facade.
    pub verifier: AuditVerifier,
    /// The genesis `KeyRotation` event to commit through the audit write funnel —
    /// content-addressed, so a replay (the row already exists) dedups to a no-op and a
    /// genesis-crash window heals. `None` only on a rotated (multi-key) anchor, where the
    /// active seed cannot rebuild the genesis signature and the rotation emitters already
    /// committed their own events transactionally.
    pub genesis_event: Option<AuditEvent>,
}

/// Why provisioning refused to enable signing. Every variant is a startup failure when
/// signing was requested — there is no degraded half-signing mode.
#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    /// Seed custody failed (missing/unsafe data dir, malformed seed, platform).
    #[error("audit seed custody failed")]
    Custody(#[from] CustodyError),
    /// The keyring file failed to load, validate, or save.
    #[error("audit keyring failed")]
    Keyring(#[from] KeyringError),
    /// The resolved seed does not match the keyring anchor's active key.
    #[error(
        "the resolved audit seed's key {resolved_pubkey} does not match the anchored \
         keyring's active key {anchored_pubkey} — refusing to sign with a different \
         identity (replace the seed or remove the keyring deliberately, never silently)"
    )]
    SeedKeyringMismatch {
        /// The public key derived from the resolved seed.
        resolved_pubkey: String,
        /// The active public key the keyring file anchors.
        anchored_pubkey: String,
    },
}

/// Resolve custody and the keyring anchor, producing the signer, verifier, and the
/// genesis event to (re-)commit.
///
/// `env_seed_b64` is the host-resolved env-custody seed (never read from the environment
/// here); `now` is the host instant, used only when this call mints a brand-new anchor.
///
/// # Errors
/// Any [`ProvisionError`]; callers treat every error as "signing unavailable" and fail
/// startup when signing was requested.
pub fn provision_audit_signing(
    data_dir: &Path,
    env_seed_b64: Option<&SecretString>,
    now: &Timestamp,
) -> Result<AuditProvision, ProvisionError> {
    let (signer, _source) = resolve_audit_signer(data_dir, env_seed_b64)?;
    // Both custody modes anchor the keyring on disk (env custody skips the seed file,
    // never the anchor), so the locked-down audit dir must exist either way.
    ensure_audit_dir(data_dir)?;

    let path = keyring_path(data_dir);
    let (keyring, genesis_event) = if path.exists() {
        let anchored = AuditKeyring::load(&path)?;
        // The resolved seed must be the anchor's ACTIVE key — the one the verifier binds
        // fresh rows to. Comparing genesis instead would admit a RETIRED genesis-seed
        // holder on a rotated keyring (it would then sign rows the active-key window
        // reads Invalid) and refuse the legitimate active-seed holder.
        let anchored_active = anchored
            .active()
            .map(|entry| entry.pubkey_b64.clone())
            .unwrap_or_default();
        if signer.public_key_b64() != anchored_active {
            return Err(ProvisionError::SeedKeyringMismatch {
                resolved_pubkey: signer.public_key_b64(),
                anchored_pubkey: anchored_active,
            });
        }
        // Single-key anchor (every v1 keyring — no rotation trigger exists): the active
        // key IS the genesis key, so the held seed can rebuild the genesis event at the
        // anchored instant as the heal copy, and the rebuilt keyring must be identical.
        let event = if anchored.keys().len() == 1 {
            let (event, rebuilt) = genesis_rotation(&signer, anchored.created_at());
            if anchored.created_at() != rebuilt.created_at() {
                return Err(ProvisionError::SeedKeyringMismatch {
                    resolved_pubkey: signer.public_key_b64(),
                    anchored_pubkey: anchored_active,
                });
            }
            Some(event)
        } else {
            // Rotated anchor: this seed cannot forge the genesis signature (by design),
            // and rotation committed its events under its own protocol — nothing to heal.
            None
        };
        (anchored, event)
    } else {
        // Genesis: anchor FIRST, then hand back the event for the caller to commit.
        let (event, fresh) = genesis_rotation(&signer, now);
        fresh.save(&path)?;
        (fresh, Some(event))
    };

    Ok(AuditProvision {
        verifier: AuditVerifier::from_keyring(keyring),
        signer,
        genesis_event,
    })
}
