//! Store configuration — the binding knobs L0 needs before the full config model lands.
//!
//! For now this carries only the embedding dimension, which is binding (data-model
//! §13.5): the vector indexes are created at this dimension and a later change is a
//! migration, not an in-place edit. A broader configuration model (paths under
//! `~/.aionforge/`, providers, tuning) will absorb this.

use std::path::PathBuf;

/// The user-space root for everything the store keeps on disk — WAL, snapshots, logs.
///
/// `~/.aionforge` by convention, resolved from `$HOME` (falling back to the current
/// directory when `$HOME` is unset, e.g. a bare service account). The forthcoming
/// configuration model lets this be overridden; the persistent constructors take an
/// explicit directory so tests stay in a temp directory and never touch this path.
#[must_use]
pub fn default_data_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".aionforge")
}

/// Like [`default_data_dir`], but refuses any `$HOME` that would resolve against the
/// current directory.
///
/// Resolves `~/.aionforge` from `$HOME`, returning `Err` when `$HOME` is unset, empty, or a
/// relative path — every case where the result would resolve against the working directory rather
/// than to the absolute `./.aionforge` only an absolute `$HOME` gives. The lenient form silently
/// falls back to a relative `./.aionforge`, which is harmless for the WAL, snapshots, and logs, but
/// unsafe to host a private signing seed: on a bare service account (a distroless container, a
/// systemd unit without `HOME`, a CI runner) it would write the key into whatever directory the
/// process happened to start in. The audit-key custody path calls this checked form and fails
/// closed, so the operator must set an explicit `data_dir` (or name an environment-variable seed)
/// rather than leak a key to the working directory.
///
/// # Errors
/// Returns a static, value-free message when `$HOME` is unset, empty, or not an absolute path.
pub fn default_data_dir_checked() -> Result<PathBuf, &'static str> {
    data_dir_checked_from(std::env::var_os("HOME"))
}

/// The pure core of [`default_data_dir_checked`], split out so the absolute / non-absolute
/// branches are testable without mutating a process-global environment variable (which would race
/// other tests in the same binary). The guard is **absoluteness**, not non-emptiness: an unset,
/// empty, or relative `$HOME` (`""`, `"."`, `"../x"`) all resolve to a working-directory-relative
/// `.aionforge`, which is exactly what the checked form exists to refuse, so all three are rejected.
fn data_dir_checked_from(home: Option<std::ffi::OsString>) -> Result<PathBuf, &'static str> {
    match home {
        Some(home) if std::path::Path::new(&home).is_absolute() => {
            Ok(PathBuf::from(home).join(".aionforge"))
        }
        _ => Err(
            "$HOME is unset or not an absolute path, so the data directory would resolve against \
             the working directory; set an explicit data_dir (or an env-var seed) before enabling \
             audit signing",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::data_dir_checked_from;
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn checked_data_dir_resolves_under_a_set_home() {
        let dir = data_dir_checked_from(Some(OsString::from("/home/agent")))
            .expect("a set HOME yields a data dir");
        assert_eq!(dir, PathBuf::from("/home/agent/.aionforge"));
    }

    #[test]
    fn checked_data_dir_refuses_a_non_absolute_home() {
        // Unset: the lenient form would fall back to a relative `./.aionforge`; the checked
        // form refuses rather than host a private seed in the working directory.
        assert!(
            data_dir_checked_from(None).is_err(),
            "unset HOME is refused"
        );
        // Empty and relative both resolve to a working-directory-relative `.aionforge`, so the
        // absoluteness guard refuses every one of them — the guarantee the docstring advertises.
        assert!(
            data_dir_checked_from(Some(OsString::new())).is_err(),
            "empty HOME is refused"
        );
        for relative in [".", "foo", "../x", "rel/path"] {
            assert!(
                data_dir_checked_from(Some(OsString::from(relative))).is_err(),
                "a relative HOME ({relative}) is refused"
            );
        }
    }
}

/// The default embedding dimension.
///
/// 1536 is interoperable across the embedders in play: codestral-embed's native size
/// and gemini-embedding's Matryoshka-truncated 1536. Changing it is a migration.
pub const DEFAULT_EMBEDDING_DIMENSION: u32 = 1536;

/// The store's binding configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreConfig {
    /// The embedding dimension every vector index is created at and checked against.
    pub embedding_dimension: u32,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            embedding_dimension: DEFAULT_EMBEDDING_DIMENSION,
        }
    }
}
