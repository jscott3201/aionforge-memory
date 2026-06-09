//! The out-of-band audit trust anchor: `<data_dir>/audit/audit_keyring.json` (06 §6).
//!
//! The keyring file is the *sole* authority on which audit-signing keys the
//! substrate has ever trusted and over which validity window each one signed.
//! Verification never bootstraps from in-store `KeyRotation` events — those are the
//! keyring's verifiable echo in the audit trail, not its source. A key absent from
//! this file is untrusted, full stop; there is no in-band heal, and a rotation that
//! crashed half-way is re-emitted by the seed-holder, never re-trusted from the
//! store.
//!
//! Entries carry no signatures of their own: a signature inside the trust root
//! cannot extend trust beyond the file's custody, which is the whole game here. The
//! "a new key is admitted only by the holder of an admitted key" rule is enforced
//! where rotations happen — only the live signer can emit and append one — and the
//! signed, canonical record of each rotation lives in the store as the audit event.
//!
//! Integrity of the file itself rides on custody, checked on every load *and* save:
//! a keyring or **parent directory** writable by group or other is refused (a
//! writable directory lets an attacker rename a forged file into place without ever
//! touching the original's bits). Public keys are not secret, so read bits are not
//! the threat — tampering is. The full `0700` directory lockdown is established by
//! [`ensure_audit_dir`](crate::audit_custody::ensure_audit_dir): file-seed custody
//! runs it on every resolve, and an env-seed deployment — which never takes the file
//! path — must run it before the first keyring save. Same-uid ownership is the
//! deployment's premise (one substrate user owns `data_dir`); the checks here are
//! mode-based, and they exist only on Unix — the supported deployment targets — with
//! keyring persistence refusing other platforms outright.
//!
//! Every timestamp is stamped in UTC ([`to_utc`]) before it is written: a
//! zone-bracketed RFC 9557 string is re-checked against the reader's tz database on
//! parse, so UTC is what makes a cross-host parse conflict unreachable.

use std::path::{Path, PathBuf};

use aionforge_domain::time::{Timestamp, to_utc};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::audit_custody::audit_dir;

/// The keyring file format version this build reads and writes.
pub const KEYRING_VERSION: u32 = 1;

/// The keyring file path: `<data_dir>/audit/audit_keyring.json`.
#[must_use]
pub fn keyring_path(data_dir: &Path) -> PathBuf {
    audit_dir(data_dir).join("audit_keyring.json")
}

/// One trusted key and its validity window `[admitted_at, retired_at)`. An open
/// `retired_at` means the key is the active one; only the last entry may be open.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyringEntry {
    /// The base64 Ed25519 verifying key.
    pub pubkey_b64: String,
    /// When the key entered service (inclusive lower bound).
    pub admitted_at: Timestamp,
    /// When its successor took over (exclusive upper bound); `None` while active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<Timestamp>,
}

/// The ordered chain of trusted audit keys, genesis first.
///
/// Fields are private and every constructor validates — [`AuditKeyring::genesis`]
/// and [`AuditKeyring::rotate`] check the key before touching state, and
/// [`AuditKeyring::load`] parses a private wire mirror and validates it before a
/// value of this type exists — so the chain invariants (contiguous half-open
/// windows, one open tail, distinct well-formed keys) hold for every keyring in
/// hand. The type deliberately does not implement `Deserialize`: that would be an
/// unvalidated public constructor for the trust anchor.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AuditKeyring {
    version: u32,
    created_at: Timestamp,
    keys: Vec<KeyringEntry>,
}

/// The unvalidated wire shape [`AuditKeyring::load`] parses before validation.
#[derive(Deserialize)]
struct RawKeyring {
    version: u32,
    created_at: Timestamp,
    keys: Vec<KeyringEntry>,
}

/// Why the keyring could not be loaded, saved, or extended. Fail-closed: a missing
/// or broken keyring means nothing verifies as trusted, never a fallback anchor.
#[derive(Debug, Error)]
pub enum KeyringError {
    /// No keyring file exists yet.
    #[error("no audit keyring at {0}")]
    Missing(PathBuf),
    /// Keyring persistence needs Unix permission bits.
    #[error("audit keyring custody requires a Unix filesystem")]
    UnsupportedPlatform,
    /// The directory holding the keyring is writable by group or other — the anchor
    /// could be replaced wholesale without touching the file's own bits.
    #[error("audit keyring directory {path} has mode {mode:o}, refusing group/other write access")]
    InsecureDir {
        /// The offending directory.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// The keyring file is writable by group or other — the anchor may have been
    /// tampered with.
    #[error("audit keyring {path} has mode {mode:o}, refusing group/other write access")]
    InsecureFile {
        /// The offending keyring file.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// A filesystem operation failed.
    #[error("audit keyring i/o on {path}: {source}")]
    Io {
        /// The path the operation touched.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The file did not parse as a keyring.
    #[error("malformed audit keyring: {0}")]
    Malformed(String),
    /// The file parsed, but was written by a format this build does not know.
    #[error(
        "audit keyring format version {0} is not supported (this build reads {KEYRING_VERSION})"
    )]
    UnsupportedVersion(u32),
    /// The key chain violates an invariant; the message names the broken rule.
    #[error("audit keyring chain invalid: {0}")]
    BrokenChain(&'static str),
}

impl AuditKeyring {
    /// A new keyring anchored on the first key. `created_at` is both the keyring's
    /// creation instant — the must-sign cutover the verifier keys on — and the
    /// genesis key's admission.
    ///
    /// # Errors
    /// [`KeyringError::BrokenChain`] when the key is not base64 for 32 bytes.
    pub fn genesis(
        genesis_pubkey_b64: String,
        created_at: &Timestamp,
    ) -> Result<Self, KeyringError> {
        well_formed_key(&genesis_pubkey_b64)?;
        let at = to_utc(created_at);
        Ok(Self {
            version: KEYRING_VERSION,
            created_at: at.clone(),
            keys: vec![KeyringEntry {
                pubkey_b64: genesis_pubkey_b64,
                admitted_at: at,
                retired_at: None,
            }],
        })
    }

    /// When the keyring (and the genesis key) entered service.
    #[must_use]
    pub fn created_at(&self) -> &Timestamp {
        &self.created_at
    }

    /// The trusted keys, genesis first.
    #[must_use]
    pub fn keys(&self) -> &[KeyringEntry] {
        &self.keys
    }

    /// The currently active key — the open-tail entry. Every constructor validates,
    /// so the chain is never empty; the `Option` mirrors `Vec::last` rather than an
    /// expected state.
    #[must_use]
    pub fn active(&self) -> Option<&KeyringEntry> {
        self.keys.last()
    }

    /// The key whose validity window `[admitted_at, retired_at)` contains `at`.
    #[must_use]
    pub fn key_for(&self, at: &Timestamp) -> Option<&KeyringEntry> {
        let instant = at.timestamp();
        self.keys.iter().find(|key| {
            key.admitted_at.timestamp() <= instant
                && key
                    .retired_at
                    .as_ref()
                    .is_none_or(|retired| instant < retired.timestamp())
        })
    }

    /// Admit a successor key at `at`: the active key's window closes at that instant
    /// and the new key's opens on it (contiguous, half-open).
    ///
    /// Every check runs before any state changes, so `Err` always leaves the keyring
    /// exactly as it was.
    ///
    /// # Errors
    /// [`KeyringError::BrokenChain`] when the new key is malformed or already in the
    /// chain, or the instant does not fall strictly after the active key's admission.
    pub fn rotate(&mut self, new_pubkey_b64: String, at: &Timestamp) -> Result<(), KeyringError> {
        well_formed_key(&new_pubkey_b64)?;
        let at = to_utc(at);
        if self.keys.iter().any(|key| key.pubkey_b64 == new_pubkey_b64) {
            return Err(KeyringError::BrokenChain(
                "a public key may enter service only once; re-admitting one would let a \
                 leaked retired seed sign again",
            ));
        }
        let Some(active) = self.keys.last_mut() else {
            return Err(KeyringError::BrokenChain("cannot rotate an empty keyring"));
        };
        if at.timestamp() <= active.admitted_at.timestamp() {
            return Err(KeyringError::BrokenChain(
                "a rotation must fall strictly after the active key's admission",
            ));
        }
        active.retired_at = Some(at.clone());
        self.keys.push(KeyringEntry {
            pubkey_b64: new_pubkey_b64,
            admitted_at: at,
            retired_at: None,
        });
        debug_assert!(
            self.validate().is_ok(),
            "rotate pre-checks cover every rule"
        );
        Ok(())
    }

    /// Check every chain invariant. A keyring built by this module always passes;
    /// this is the gate a parsed file must clear before anything trusts it.
    ///
    /// # Errors
    /// [`KeyringError::UnsupportedVersion`] or [`KeyringError::BrokenChain`] naming
    /// the violated rule.
    pub fn validate(&self) -> Result<(), KeyringError> {
        if self.version != KEYRING_VERSION {
            return Err(KeyringError::UnsupportedVersion(self.version));
        }
        let Some((first, rest)) = self.keys.split_first() else {
            return Err(KeyringError::BrokenChain(
                "a keyring holds at least its genesis key",
            ));
        };
        if first.admitted_at.timestamp() != self.created_at.timestamp() {
            return Err(KeyringError::BrokenChain(
                "the genesis key is admitted at the keyring's creation instant",
            ));
        }
        let mut previous = first;
        for key in rest {
            let Some(retired) = previous.retired_at.as_ref() else {
                return Err(KeyringError::BrokenChain(
                    "only the last key's window may be open",
                ));
            };
            if retired.timestamp() <= previous.admitted_at.timestamp() {
                return Err(KeyringError::BrokenChain(
                    "every closed window spans a positive duration",
                ));
            }
            if key.admitted_at.timestamp() != retired.timestamp() {
                return Err(KeyringError::BrokenChain(
                    "each key is admitted at the instant its predecessor retires",
                ));
            }
            previous = key;
        }
        if previous.retired_at.is_some() {
            return Err(KeyringError::BrokenChain(
                "the last key's window stays open",
            ));
        }
        for key in &self.keys {
            well_formed_key(&key.pubkey_b64)?;
        }
        let mut seen: Vec<&str> = self.keys.iter().map(|k| k.pubkey_b64.as_str()).collect();
        seen.sort_unstable();
        seen.dedup();
        if seen.len() != self.keys.len() {
            return Err(KeyringError::BrokenChain(
                "a public key may enter service only once",
            ));
        }
        Ok(())
    }

    /// Load and validate the keyring. A missing file is [`KeyringError::Missing`] —
    /// the caller decides what that means (the verifier fails closed; first enable
    /// creates a genesis). The custody checks (file and parent-directory write bits)
    /// run on Unix only — the supported deployment targets.
    ///
    /// # Errors
    /// Any [`KeyringError`]; nothing is created or repaired on the load path.
    pub fn load(path: &Path) -> Result<Self, KeyringError> {
        let text = read_keyring_text(path)?;
        let raw: RawKeyring =
            serde_json::from_str(&text).map_err(|err| KeyringError::Malformed(err.to_string()))?;
        let keyring = Self {
            version: raw.version,
            created_at: raw.created_at,
            keys: raw.keys,
        };
        keyring.validate()?;
        Ok(keyring)
    }

    /// Persist the keyring atomically (sibling temp file, sync, rename) with owner-
    /// only access, validating first so an invalid chain can never reach disk.
    ///
    /// # Errors
    /// Validation errors, [`KeyringError::UnsupportedPlatform`] off Unix, or
    /// [`KeyringError::Io`].
    pub fn save(&self, path: &Path) -> Result<(), KeyringError> {
        self.validate()?;
        write_keyring_text(path, &self.render())
    }

    fn render(&self) -> String {
        let mut text = serde_json::to_string_pretty(self)
            .expect("a keyring of strings and timestamps serializes to JSON");
        text.push('\n');
        text
    }
}

/// A trusted key must be base64 for exactly 32 bytes — checked before any
/// constructor touches state and again on every stored entry during validation.
fn well_formed_key(pubkey_b64: &str) -> Result<(), KeyringError> {
    let decoded = BASE64
        .decode(pubkey_b64)
        .map_err(|_| KeyringError::BrokenChain("a stored public key must be base64"))?;
    if decoded.len() != 32 {
        return Err(KeyringError::BrokenChain(
            "a stored public key must decode to 32 bytes",
        ));
    }
    Ok(())
}

/// Refuse a group/other-writable parent directory: a writable directory lets a
/// forged keyring be renamed into place without touching the file's own bits.
#[cfg(unix)]
fn refuse_writable_parent(path: &Path) -> Result<(), KeyringError> {
    use std::os::unix::fs::PermissionsExt;
    let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };
    let metadata = std::fs::metadata(dir).map_err(|source| KeyringError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o022 != 0 {
        return Err(KeyringError::InsecureDir {
            path: dir.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

fn read_keyring_text(path: &Path) -> Result<String, KeyringError> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(KeyringError::Missing(path.to_path_buf()));
        }
        Err(source) => {
            return Err(KeyringError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        refuse_writable_parent(path)?;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o022 != 0 {
            return Err(KeyringError::InsecureFile {
                path: path.to_path_buf(),
                mode,
            });
        }
    }
    #[cfg(not(unix))]
    let _ = metadata;
    std::fs::read_to_string(path).map_err(|source| KeyringError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(unix)]
fn write_keyring_text(path: &Path, text: &str) -> Result<(), KeyringError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    refuse_writable_parent(path)?;
    let io_err = |p: &Path| {
        let p = p.to_path_buf();
        move |source| KeyringError::Io { path: p, source }
    };
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(io_err(&tmp))?;
    file.write_all(text.as_bytes()).map_err(io_err(&tmp))?;
    file.sync_all().map_err(io_err(&tmp))?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(io_err(path))?;
    let dir = path
        .parent()
        .expect("keyring path always has the audit dir parent");
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io_err(dir))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_keyring_text(_path: &Path, _text: &str) -> Result<(), KeyringError> {
    Err(KeyringError::UnsupportedPlatform)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn at(text: &str) -> Timestamp {
        text.parse().expect("valid zoned datetime")
    }

    fn pubkey(byte: u8) -> String {
        BASE64.encode([byte; 32])
    }

    fn genesis_ok(byte: u8, created: &str) -> AuditKeyring {
        AuditKeyring::genesis(pubkey(byte), &at(created)).expect("a well-formed genesis key")
    }

    fn temp_keyring_path(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aionforge-keyring-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        // Deterministic regardless of the runner's umask: the parent-dir write-bit
        // check would otherwise flake on a umask-002 host.
        set_mode(&dir, 0o700);
        dir.join("audit_keyring.json")
    }

    #[test]
    fn a_genesis_keyring_round_trips_in_utc() {
        let keyring =
            AuditKeyring::genesis(pubkey(1), &at("2026-06-09T09:00:00-05:00[America/Chicago]"))
                .expect("a well-formed genesis key");
        keyring.validate().expect("genesis is valid");

        let path = temp_keyring_path("genesis");
        keyring.save(&path).expect("saves");
        let loaded = AuditKeyring::load(&path).expect("loads");
        assert_eq!(loaded, keyring);

        // Stamped in UTC on the wire, so a reader's tz database never gets a vote.
        let text = std::fs::read_to_string(&path).expect("readable");
        assert!(
            text.contains("+00:00[UTC]"),
            "keyring stamps UTC, got:\n{text}"
        );
        assert!(!text.contains("America/Chicago"));
    }

    #[test]
    fn rotation_closes_the_old_window_on_the_new_admission() {
        let mut keyring = genesis_ok(1, "2026-06-09T14:00:00+00:00[UTC]");
        keyring
            .rotate(pubkey(2), &at("2026-07-01T00:00:00+00:00[UTC]"))
            .expect("rotates");

        let keys = keyring.keys();
        assert_eq!(keys.len(), 2);
        assert_eq!(
            keys[0].retired_at.as_ref().map(jiff::Zoned::timestamp),
            Some(keys[1].admitted_at.timestamp()),
            "windows are contiguous"
        );
        assert_eq!(
            keyring.active().map(|k| k.pubkey_b64.as_str()),
            Some(pubkey(2)).as_deref()
        );

        // Half-open windows: the rotation instant belongs to the successor.
        let rotation_instant = at("2026-07-01T00:00:00+00:00[UTC]");
        assert_eq!(
            keyring
                .key_for(&rotation_instant)
                .map(|k| k.pubkey_b64.as_str()),
            Some(pubkey(2)).as_deref()
        );
        let before = at("2026-06-15T00:00:00+00:00[UTC]");
        assert_eq!(
            keyring.key_for(&before).map(|k| k.pubkey_b64.as_str()),
            Some(pubkey(1)).as_deref()
        );
        let prehistory = at("2026-01-01T00:00:00+00:00[UTC]");
        assert!(keyring.key_for(&prehistory).is_none());
    }

    #[test]
    fn a_key_may_enter_service_only_once() {
        let mut keyring = genesis_ok(1, "2026-06-09T14:00:00+00:00[UTC]");
        let err = keyring
            .rotate(pubkey(1), &at("2026-07-01T00:00:00+00:00[UTC]"))
            .expect_err("re-admitting the genesis key is refused");
        assert!(matches!(err, KeyringError::BrokenChain(_)));
    }

    #[test]
    fn a_rotation_must_move_time_forward() {
        let mut keyring = genesis_ok(1, "2026-06-09T14:00:00+00:00[UTC]");
        let err = keyring
            .rotate(pubkey(2), &at("2026-06-09T14:00:00+00:00[UTC]"))
            .expect_err("a zero-width genesis window is refused");
        assert!(matches!(err, KeyringError::BrokenChain(_)));
    }

    /// Every `Err` path leaves the keyring untouched — a caller that logs a failed
    /// rotation and carries on must still hold a valid chain.
    #[test]
    fn a_failed_rotation_leaves_the_keyring_unchanged() {
        let mut keyring = genesis_ok(1, "2026-06-09T14:00:00+00:00[UTC]");
        let pristine = keyring.clone();
        for bad in ["not base64 !!!", &BASE64.encode([0u8; 16])] {
            keyring
                .rotate(bad.to_string(), &at("2026-07-01T00:00:00+00:00[UTC]"))
                .expect_err("a malformed successor key is refused");
            assert_eq!(keyring, pristine, "no half-mutation on Err");
        }
        AuditKeyring::genesis("garbage".to_string(), &at("2026-06-09T14:00:00+00:00[UTC]"))
            .expect_err("a malformed genesis key is refused");
    }

    /// One JSON mutation per chain rule, each loaded through the real file path:
    /// deleting any rule from validate() must turn at least one row green.
    #[test]
    fn every_chain_invariant_is_enforced_on_load() {
        let mut keyring = genesis_ok(1, "2026-06-09T14:00:00+00:00[UTC]");
        keyring
            .rotate(pubkey(2), &at("2026-07-01T00:00:00+00:00[UTC]"))
            .expect("rotates");
        let path = temp_keyring_path("invariants");
        keyring.save(&path).expect("saves");
        let pristine = std::fs::read_to_string(&path).expect("readable");

        type Mutation = fn(&mut serde_json::Value);
        let mutations: [(&str, Mutation); 7] = [
            ("empty key list", |v| {
                v["keys"] = serde_json::json!([]);
            }),
            ("genesis admission != keyring creation", |v| {
                v["created_at"] = serde_json::json!("2026-06-09T13:00:00+00:00[UTC]");
            }),
            ("mid-chain open window", |v| {
                v["keys"][0]
                    .as_object_mut()
                    .expect("entry object")
                    .remove("retired_at");
            }),
            ("zero-width window", |v| {
                v["keys"][0]["retired_at"] = serde_json::json!("2026-06-09T14:00:00+00:00[UTC]");
            }),
            ("closed tail window", |v| {
                v["keys"][1]["retired_at"] = serde_json::json!("2026-08-01T00:00:00+00:00[UTC]");
            }),
            ("non-base64 stored key", |v| {
                v["keys"][1]["pubkey_b64"] = serde_json::json!("not base64 !!!");
            }),
            ("wrong-length stored key", |v| {
                v["keys"][1]["pubkey_b64"] =
                    serde_json::json!(base64::engine::general_purpose::STANDARD.encode([0u8; 16]));
            }),
        ];
        for (rule, mutate) in mutations {
            let mut value: serde_json::Value =
                serde_json::from_str(&pristine).expect("pristine parses");
            mutate(&mut value);
            std::fs::write(&path, serde_json::to_string(&value).expect("renders"))
                .expect("write mutation");
            set_mode(&path, 0o600);
            let err = AuditKeyring::load(&path)
                .expect_err(&format!("a chain violating '{rule}' must be refused"));
            assert!(
                matches!(err, KeyringError::BrokenChain(_)),
                "'{rule}' should be BrokenChain, got: {err}"
            );
        }

        // The duplicate-key sweep needs distinct windows with the same key.
        let mut value: serde_json::Value = serde_json::from_str(&pristine).expect("parses");
        value["keys"][1]["pubkey_b64"] = value["keys"][0]["pubkey_b64"].clone();
        std::fs::write(&path, serde_json::to_string(&value).expect("renders")).expect("write");
        set_mode(&path, 0o600);
        let err = AuditKeyring::load(&path).expect_err("a duplicated key must be refused");
        assert!(matches!(err, KeyringError::BrokenChain(_)));
    }

    #[test]
    fn a_tampered_chain_fails_to_load() {
        let mut keyring = genesis_ok(1, "2026-06-09T14:00:00+00:00[UTC]");
        keyring
            .rotate(pubkey(2), &at("2026-07-01T00:00:00+00:00[UTC]"))
            .expect("rotates");
        let path = temp_keyring_path("tampered");
        keyring.save(&path).expect("saves");

        // Widen the retired key's window past its successor's admission — the kind of
        // edit a leaked old seed would profit from.
        let text = std::fs::read_to_string(&path).expect("readable");
        let mut value: serde_json::Value = serde_json::from_str(&text).expect("parses");
        value["keys"][0]["retired_at"] = serde_json::json!("2026-08-01T00:00:00+00:00[UTC]");
        std::fs::write(&path, serde_json::to_string(&value).expect("renders")).expect("tamper");
        set_mode(&path, 0o600);
        let err = AuditKeyring::load(&path).expect_err("a broken chain is refused");
        assert!(matches!(err, KeyringError::BrokenChain(_)));
    }

    #[test]
    fn missing_and_malformed_files_fail_closed() {
        let path = temp_keyring_path("missing");
        assert!(matches!(
            AuditKeyring::load(&path),
            Err(KeyringError::Missing(_))
        ));

        std::fs::write(&path, "not json").expect("write");
        set_mode(&path, 0o600);
        assert!(matches!(
            AuditKeyring::load(&path),
            Err(KeyringError::Malformed(_))
        ));
    }

    #[test]
    fn an_unknown_format_version_is_refused() {
        let keyring = genesis_ok(1, "2026-06-09T14:00:00+00:00[UTC]");
        let path = temp_keyring_path("version");
        keyring.save(&path).expect("saves");
        let text = std::fs::read_to_string(&path).expect("readable");
        std::fs::write(&path, text.replace("\"version\": 1", "\"version\": 2")).expect("write");
        set_mode(&path, 0o600);
        assert!(matches!(
            AuditKeyring::load(&path),
            Err(KeyringError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn a_group_writable_keyring_is_refused() {
        let keyring = genesis_ok(1, "2026-06-09T14:00:00+00:00[UTC]");
        let path = temp_keyring_path("writable");
        keyring.save(&path).expect("saves");
        set_mode(&path, 0o664);
        assert!(matches!(
            AuditKeyring::load(&path),
            Err(KeyringError::InsecureFile { mode: 0o664, .. })
        ));
        // Group/other READ is allowed: public keys are not secret, tampering is the threat.
        set_mode(&path, 0o644);
        AuditKeyring::load(&path).expect("a read-only-shared keyring loads");
    }

    /// A writable parent directory is the rename-replace tamper vector: refused on
    /// load AND on save, regardless of the file's own bits.
    #[test]
    fn a_group_writable_parent_directory_is_refused() {
        let keyring = genesis_ok(1, "2026-06-09T14:00:00+00:00[UTC]");
        let path = temp_keyring_path("writable-dir");
        keyring.save(&path).expect("saves under a 0700 dir");

        let dir = path.parent().expect("temp parent").to_path_buf();
        set_mode(&dir, 0o775);
        assert!(matches!(
            AuditKeyring::load(&path),
            Err(KeyringError::InsecureDir { mode: 0o775, .. })
        ));
        assert!(matches!(
            keyring.save(&path),
            Err(KeyringError::InsecureDir { .. })
        ));
        set_mode(&dir, 0o700);
        AuditKeyring::load(&path).expect("loads again once the directory is tight");
    }

    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }
}
