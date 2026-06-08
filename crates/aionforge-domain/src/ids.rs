//! Stable identifiers, content hashes, and retrieval serialization ids (02 §10).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::DomainError;

/// A stable, collision-resistant, never-reused external identifier (a UUID).
///
/// A generated id is a UUIDv7 — time-ordered, so ids sort by creation time and the
/// substrate's native UUID index keeps them in chronological order. A content-addressed
/// id (see [`Id::from_content_hash`]) is a UUIDv8. The value is a `Copy` 128-bit UUID,
/// safe to expose; the substrate never derives an `Id` from a storage position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Id(uuid::Uuid);

impl Id {
    /// Generate a fresh, time-sortable identifier (UUIDv7).
    ///
    /// The embedded millisecond timestamp is read from the system clock, the same as
    /// the ULID this replaced. An id's own timestamp is an opaque sort key, never read
    /// back as a domain time, so this is the one identifier path that touches the wall
    /// clock; every stored temporal field still takes an explicit timestamp instead.
    #[must_use]
    pub fn generate() -> Self {
        Self(uuid::Uuid::now_v7())
    }

    /// Derive a deterministic identifier from canonical content bytes (a UUIDv8).
    ///
    /// Unlike [`Id::generate`], the result is a pure function of the input: the same
    /// bytes always yield the same id. Consolidation uses this to give an extracted
    /// fact a stable identity (over namespace, subject, predicate, object, source
    /// episode, and rule version) so re-running a cursor position dedups to a no-op
    /// rather than duplicating the assertion (04 §2). The blake3 digest's leading 128
    /// bits become a UUIDv8 — the RFC's "custom" version for application-defined bytes.
    /// Six of those bits carry the version and variant, leaving 122 bits of hash, which
    /// keeps derived ids collision-free with room to spare. A v8 id holds no timestamp,
    /// so it is correctly not time-sortable.
    #[must_use]
    pub fn from_content_hash(bytes: &[u8]) -> Self {
        let digest = blake3::hash(bytes);
        let mut packed = [0u8; 16];
        packed.copy_from_slice(&digest.as_bytes()[..16]);
        Self(uuid::Builder::from_custom_bytes(packed).into_uuid())
    }

    /// Construct an identifier from an existing string, validating its UUID shape.
    ///
    /// # Errors
    /// Returns [`DomainError::InvalidId`] if the string is not a valid UUID.
    pub fn parse(s: impl Into<String>) -> Result<Self, DomainError> {
        let s = s.into();
        uuid::Uuid::parse_str(&s)
            .map(Self)
            .map_err(|_| DomainError::InvalidId(s))
    }

    /// The underlying UUID, for the storage layer that binds it as a native value.
    #[must_use]
    pub fn as_uuid(&self) -> uuid::Uuid {
        self.0
    }

    /// Wrap a UUID read back from the store.
    #[must_use]
    pub fn from_uuid(uuid: uuid::Uuid) -> Self {
        Self(uuid)
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_id_is_deterministic() {
        let a = Id::from_content_hash(b"ns|alice|works_on|aionforge|ep01|v1");
        let b = Id::from_content_hash(b"ns|alice|works_on|aionforge|ep01|v1");
        assert_eq!(a, b);
    }

    #[test]
    fn content_hash_id_separates_distinct_inputs() {
        let a = Id::from_content_hash(b"ns|alice|works_on|aionforge|ep01|v1");
        let b = Id::from_content_hash(b"ns|alice|works_on|selene|ep01|v1");
        assert_ne!(a, b);
    }

    #[test]
    fn content_hash_id_round_trips_through_parse() {
        // A derived id must round-trip through the same validation as any boundary
        // id, so the store can persist it without a special path.
        let derived = Id::from_content_hash(b"anything");
        assert!(Id::parse(derived.to_string()).is_ok());
    }

    #[test]
    fn a_generated_id_is_uuid_v7() {
        // Generated ids carry a timestamp, so they sort by creation time.
        assert_eq!(Id::generate().as_uuid().get_version_num(), 7);
    }

    #[test]
    fn a_content_addressed_id_is_uuid_v8() {
        // Derived ids are the RFC "custom" version: deterministic, not time-sortable.
        assert_eq!(
            Id::from_content_hash(b"anything")
                .as_uuid()
                .get_version_num(),
            8
        );
    }
}
