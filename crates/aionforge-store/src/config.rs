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
