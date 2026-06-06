//! Stable identifiers, content hashes, and retrieval serialization ids (02 §10).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::DomainError;

/// A stable, collision-resistant, never-reused external identifier (ULID-shaped).
///
/// Sortable by creation time and safe to expose; the substrate never derives an
/// `Id` from a storage position.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Id(String);

impl Id {
    /// Generate a fresh, monotonic, sortable identifier.
    #[must_use]
    pub fn generate() -> Self {
        Self(ulid::Ulid::new().to_string())
    }

    /// Construct an identifier from an existing string, validating its ULID shape.
    ///
    /// # Errors
    /// Returns [`DomainError::InvalidId`] if the string is not a valid ULID.
    pub fn parse(s: impl Into<String>) -> Result<Self, DomainError> {
        let s = s.into();
        ulid::Ulid::from_string(&s).map_err(|_| DomainError::InvalidId(s.clone()))?;
        Ok(Self(s))
    }

    /// The identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Id {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A blake3 content hash, hex-encoded; the dedup and change-detection key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentHash(String);

impl ContentHash {
    /// Hash already-normalized content bytes with blake3 and hex-encode.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }

    /// Reconstruct a content hash from a stored blake3 hex digest.
    ///
    /// # Errors
    /// Returns [`DomainError::InvalidContentHash`] unless the string is exactly 64
    /// lowercase hexadecimal characters — the form [`ContentHash::of`] produces.
    pub fn from_hex(hex: impl Into<String>) -> Result<Self, DomainError> {
        let hex = hex.into();
        let well_formed = hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if well_formed {
            Ok(Self(hex))
        } else {
            Err(DomainError::InvalidContentHash(hex))
        }
    }

    /// The hex string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A content-derived key (a blake3 prefix over a per-kind canonical key) that
/// makes rendered recall text stable across small variations of the same memory,
/// preserving the prefix-cache contract (03 §6).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SerializationId(String);

impl SerializationId {
    /// Derive a serialization id from a per-kind tag and canonical key bytes.
    ///
    /// The tag namespaces the hash so two kinds with the same canonical key bytes
    /// never collide. The result is a 64-bit (16 hex character) prefix.
    #[must_use]
    pub fn derive(kind_tag: &str, canonical_key: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(kind_tag.as_bytes());
        hasher.update(&[0]);
        hasher.update(canonical_key);
        let hex = hasher.finalize().to_hex();
        Self(hex.as_str()[..16].to_string())
    }

    /// The id string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SerializationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
