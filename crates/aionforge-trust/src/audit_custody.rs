//! Where the substrate's audit-signing seed lives between runs (06 §6).
//!
//! Two custody forms, in precedence order:
//!
//! 1. **Environment escalation** — the operator names an env variable holding the
//!    base64 seed (`security.audit_key_env`). The seed never touches disk; this
//!    module never even resolves a path.
//! 2. **Self-custody file** — the default: raw 32 bytes at
//!    `<data_dir>/audit/audit_seed`, minted on first enable. The `audit/` directory
//!    is created `0700` and the file `0600` *at creation* (no chmod window), and a
//!    pre-existing directory or file with looser permissions is refused, not
//!    silently tightened — the operator finds out something else touched the key.
//!
//! File custody is Unix-only: without real permission bits the lockdown is
//! unenforceable, so other platforms get [`CustodyError::UnsupportedPlatform`]
//! rather than a world-readable key. A relative `data_dir` is refused outright
//! ([`CustodyError::UnsafeDataDir`]) so a key can never land wherever the process
//! happened to be started — pair with the store's `default_data_dir_checked`.
//!
//! Concurrency: custody assumes one substrate process per `data_dir` (the engine's
//! own model). Two processes racing first-enable could each mint a key and one
//! write would win; nothing here arbitrates that.

use std::path::{Path, PathBuf};

use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;

use crate::audit_signer::{AuditSigner, KeyError, SecretSeed};

/// The audit key directory: `<data_dir>/audit`.
#[must_use]
pub fn audit_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("audit")
}

/// The seed file path: `<data_dir>/audit/audit_seed`.
#[must_use]
pub fn seed_path(data_dir: &Path) -> PathBuf {
    audit_dir(data_dir).join("audit_seed")
}

/// How the resolved signer's seed was obtained — for logs and the doctor surface,
/// never a secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedSource {
    /// Decoded from the operator-named environment variable; disk untouched.
    EnvVar,
    /// Loaded from an existing seed file.
    FileLoaded,
    /// Freshly minted and persisted to a new seed file (first enable).
    FileMinted,
}

/// Why the audit seed could not be resolved. Fail-closed: every variant means
/// "signing stays off and startup reports why", never a silent fallback.
#[derive(Debug, Error)]
pub enum CustodyError {
    /// The data directory is not an absolute path — refusing to drop a key where
    /// the process happens to be running.
    #[error("refusing audit-key custody under a non-absolute data_dir: {0}")]
    UnsafeDataDir(PathBuf),
    /// File custody needs Unix permission bits; refusing a world-readable key.
    #[error("audit-key file custody requires a Unix filesystem")]
    UnsupportedPlatform,
    /// The environment seed was malformed.
    #[error("environment audit seed: {0}")]
    Seed(#[from] KeyError),
    /// No seed file exists (load-only paths; the resolve path mints instead).
    #[error("no audit seed file at {0}")]
    SeedFileMissing(PathBuf),
    /// The seed file exists but is not exactly 32 bytes.
    #[error("audit seed file {path} holds {len} bytes, expected exactly 32")]
    SeedFileWrongLength {
        /// The offending seed file.
        path: PathBuf,
        /// Its actual length in bytes.
        len: u64,
    },
    /// The seed file is readable by group or other.
    #[error("audit seed file {path} has mode {mode:o}, refusing anything looser than 0600")]
    InsecureSeedFile {
        /// The offending seed file.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// The audit directory is accessible by group or other.
    #[error("audit key directory {path} has mode {mode:o}, refusing anything looser than 0700")]
    InsecureKeyDir {
        /// The offending directory.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// A filesystem operation failed.
    #[error("audit-key custody i/o on {path}: {source}")]
    Io {
        /// The path the operation touched.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
}

/// Resolve the audit signer from custody: the env seed when one is configured,
/// otherwise the seed file — loaded if present, minted and persisted on first
/// enable.
///
/// The env form never touches disk (and so never needs `data_dir`); the file form
/// enforces the full lockdown described in the module docs.
///
/// # Errors
/// Any [`CustodyError`]; see the variants. Callers treat every error as "signing
/// unavailable" and fail startup when signing was requested.
pub fn resolve_audit_signer(
    data_dir: &Path,
    env_seed_b64: Option<&SecretString>,
) -> Result<(AuditSigner, SeedSource), CustodyError> {
    if let Some(secret) = env_seed_b64 {
        let seed = SecretSeed::from_base64(secret.expose_secret())?;
        return Ok((AuditSigner::from_seed(&seed), SeedSource::EnvVar));
    }
    file_custody(data_dir)
}

/// Load the persisted seed without ever minting — the CLI export path. Errors with
/// [`CustodyError::SeedFileMissing`] when no seed has been minted yet.
///
/// # Errors
/// Any file-custody [`CustodyError`]; never creates or repairs anything.
pub fn load_audit_seed(data_dir: &Path) -> Result<SecretSeed, CustodyError> {
    #[cfg(unix)]
    {
        if !data_dir.is_absolute() {
            return Err(CustodyError::UnsafeDataDir(data_dir.to_path_buf()));
        }
        read_seed_file(&seed_path(data_dir))
    }
    #[cfg(not(unix))]
    {
        let _ = data_dir;
        Err(CustodyError::UnsupportedPlatform)
    }
}

/// Create (with `0700` set at creation) or vet the audit key directory, returning
/// its path. File custody runs this on every resolve; an env-seed deployment — which
/// never takes the file path — calls it itself before the first keyring save, since
/// the keyring lives in this directory regardless of where the seed does.
///
/// # Errors
/// [`CustodyError::UnsafeDataDir`] for a relative `data_dir`,
/// [`CustodyError::UnsupportedPlatform`] off Unix, [`CustodyError::InsecureKeyDir`]
/// for a pre-existing directory with group/other access, or [`CustodyError::Io`].
pub fn ensure_audit_dir(data_dir: &Path) -> Result<PathBuf, CustodyError> {
    #[cfg(unix)]
    {
        if !data_dir.is_absolute() {
            return Err(CustodyError::UnsafeDataDir(data_dir.to_path_buf()));
        }
        std::fs::create_dir_all(data_dir).map_err(|source| CustodyError::Io {
            path: data_dir.to_path_buf(),
            source,
        })?;
        let dir = audit_dir(data_dir);
        lockdown_audit_dir(&dir)?;
        Ok(dir)
    }
    #[cfg(not(unix))]
    {
        let _ = data_dir;
        Err(CustodyError::UnsupportedPlatform)
    }
}

#[cfg(unix)]
fn file_custody(data_dir: &Path) -> Result<(AuditSigner, SeedSource), CustodyError> {
    ensure_audit_dir(data_dir)?;

    let path = seed_path(data_dir);
    match read_seed_file(&path) {
        Ok(seed) => Ok((AuditSigner::from_seed(&seed), SeedSource::FileLoaded)),
        Err(CustodyError::SeedFileMissing(_)) => {
            let (signer, seed) = AuditSigner::mint();
            write_seed_file(&path, &seed)?;
            Ok((signer, SeedSource::FileMinted))
        }
        Err(err) => Err(err),
    }
}

#[cfg(not(unix))]
fn file_custody(_data_dir: &Path) -> Result<(AuditSigner, SeedSource), CustodyError> {
    Err(CustodyError::UnsupportedPlatform)
}

/// Create the audit directory `0700` at creation time (no window where it exists
/// with umask permissions); if it already exists, refuse group/other access bits.
#[cfg(unix)]
fn lockdown_audit_dir(dir: &Path) -> Result<(), CustodyError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let meta = std::fs::metadata(dir).map_err(|source| CustodyError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(CustodyError::InsecureKeyDir {
                    path: dir.to_path_buf(),
                    mode,
                });
            }
            Ok(())
        }
        Err(source) => Err(CustodyError::Io {
            path: dir.to_path_buf(),
            source,
        }),
    }
}

/// Read the 32-byte seed, refusing a loose mode or a wrong size before touching
/// content. Reads into a wiped buffer; never loads more than 33 bytes.
#[cfg(unix)]
fn read_seed_file(path: &Path) -> Result<SecretSeed, CustodyError> {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    use zeroize::Zeroizing;

    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(CustodyError::SeedFileMissing(path.to_path_buf()));
        }
        Err(source) => {
            return Err(CustodyError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(CustodyError::InsecureSeedFile {
            path: path.to_path_buf(),
            mode,
        });
    }
    if meta.len() != 32 {
        return Err(CustodyError::SeedFileWrongLength {
            path: path.to_path_buf(),
            len: meta.len(),
        });
    }

    let io_err = |source| CustodyError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut file = std::fs::File::open(path).map_err(io_err)?;
    let mut buf = Zeroizing::new([0u8; 32]);
    file.read_exact(buf.as_mut()).map_err(io_err)?;
    // The metadata length was a point-in-time read; confirm EOF on the open handle,
    // and re-stat that handle for the true length if the file grew under us.
    let mut probe = [0u8; 1];
    if file.read(&mut probe).map_err(io_err)? != 0 {
        let len = file.metadata().map_or(meta.len() + 1, |m| m.len());
        return Err(CustodyError::SeedFileWrongLength {
            path: path.to_path_buf(),
            len,
        });
    }
    Ok(SecretSeed::from_bytes(*buf))
}

/// Persist the seed `0600` at creation time, atomically: written to a sibling temp
/// file, synced, then renamed into place, so a crash never leaves a partial seed
/// under the real name.
#[cfg(unix)]
fn write_seed_file(path: &Path, seed: &SecretSeed) -> Result<(), CustodyError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let io_err = |p: &Path| {
        let p = p.to_path_buf();
        move |source| CustodyError::Io { path: p, source }
    };
    let tmp = path.with_extension("tmp");
    // A leftover temp file is debris from a crashed mint; clear it.
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(io_err(&tmp))?;
    file.write_all(seed.as_bytes()).map_err(io_err(&tmp))?;
    file.sync_all().map_err(io_err(&tmp))?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(io_err(path))?;
    // Sync the directory so the rename itself is durable.
    let dir = path
        .parent()
        .expect("seed path always has the audit dir parent");
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io_err(dir))?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fresh, empty temp directory unique to `label`, removed first so re-runs
    /// start clean. No external temp-dir crate, matching the store's durable tests.
    fn temp_data_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aionforge-custody-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("path exists")
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn an_env_seed_wins_and_never_touches_disk() {
        let data_dir = temp_data_dir("env-wins");
        let secret = SecretString::from(base64_of([9u8; 32]));
        let (signer, source) =
            resolve_audit_signer(&data_dir, Some(&secret)).expect("env seed resolves");
        assert_eq!(source, SeedSource::EnvVar);
        assert_eq!(
            signer.public_key_b64(),
            AuditSigner::from_seed(&SecretSeed::from_bytes([9u8; 32])).public_key_b64()
        );
        assert!(
            !data_dir.exists(),
            "env custody must not create the data dir"
        );
    }

    #[test]
    fn a_malformed_env_seed_fails_closed() {
        let data_dir = temp_data_dir("env-bad");
        let secret = SecretString::from("not base64 !!!");
        assert!(matches!(
            resolve_audit_signer(&data_dir, Some(&secret)),
            Err(CustodyError::Seed(KeyError::SeedNotBase64))
        ));
        assert!(
            !data_dir.exists(),
            "a refused env seed must not fall back to disk"
        );
    }

    #[test]
    fn a_relative_data_dir_is_refused_for_file_custody() {
        assert!(matches!(
            resolve_audit_signer(Path::new("relative/dir"), None),
            Err(CustodyError::UnsafeDataDir(_))
        ));
        assert!(matches!(
            load_audit_seed(Path::new(".")),
            Err(CustodyError::UnsafeDataDir(_))
        ));
    }

    /// The custody round-trip: first enable mints and persists under the lockdown
    /// modes; the next start loads the very same key.
    #[test]
    fn first_enable_mints_then_every_start_loads_the_same_key() {
        let data_dir = temp_data_dir("mint-load");
        let (minted, source) = resolve_audit_signer(&data_dir, None).expect("mints");
        assert_eq!(source, SeedSource::FileMinted);
        assert_eq!(mode_of(&audit_dir(&data_dir)), 0o700);
        assert_eq!(mode_of(&seed_path(&data_dir)), 0o600);

        let (loaded, source) = resolve_audit_signer(&data_dir, None).expect("loads");
        assert_eq!(source, SeedSource::FileLoaded);
        assert_eq!(minted.public_key_b64(), loaded.public_key_b64());

        let exported = load_audit_seed(&data_dir).expect("export loads");
        assert_eq!(
            AuditSigner::from_seed(&exported).public_key_b64(),
            minted.public_key_b64()
        );
    }

    #[test]
    fn a_wrong_size_seed_file_is_refused_not_repaired() {
        let data_dir = temp_data_dir("short-file");
        resolve_audit_signer(&data_dir, None).expect("mints");
        std::fs::write(seed_path(&data_dir), [0u8; 31]).expect("truncate");
        set_mode(&seed_path(&data_dir), 0o600);
        assert!(matches!(
            resolve_audit_signer(&data_dir, None),
            Err(CustodyError::SeedFileWrongLength { len: 31, .. })
        ));
    }

    #[test]
    fn a_loose_seed_file_mode_is_refused() {
        let data_dir = temp_data_dir("loose-file");
        resolve_audit_signer(&data_dir, None).expect("mints");
        set_mode(&seed_path(&data_dir), 0o644);
        assert!(matches!(
            resolve_audit_signer(&data_dir, None),
            Err(CustodyError::InsecureSeedFile { mode: 0o644, .. })
        ));
    }

    #[test]
    fn a_loose_audit_dir_mode_is_refused() {
        let data_dir = temp_data_dir("loose-dir");
        resolve_audit_signer(&data_dir, None).expect("mints");
        set_mode(&audit_dir(&data_dir), 0o755);
        assert!(matches!(
            resolve_audit_signer(&data_dir, None),
            Err(CustodyError::InsecureKeyDir { mode: 0o755, .. })
        ));
    }

    #[test]
    fn load_only_never_mints() {
        let data_dir = temp_data_dir("load-only");
        assert!(matches!(
            load_audit_seed(&data_dir),
            Err(CustodyError::SeedFileMissing(_))
        ));
        assert!(
            !seed_path(&data_dir).exists(),
            "the export path must not create a key"
        );
    }

    fn base64_of(bytes: [u8; 32]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn set_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }
}
