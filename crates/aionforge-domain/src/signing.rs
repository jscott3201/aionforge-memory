//! Canonical byte encodings for provenance, attestation, and audit signatures
//! (02 §10, 06 §6).
//!
//! These signatures are computed over a fixed, versioned canonical byte encoding so
//! verification is reproducible across writers and releases. This module produces
//! only the *payload bytes*; the Ed25519 signing and verification live in the trust
//! layer (M4/M6), keeping this crate free of I/O and crypto. The encoding is
//! domain-separated (a per-purpose tag) and length-prefixed (a `u32` before each
//! field), so neither a cross-protocol reuse nor a field-boundary ambiguity can
//! produce a colliding payload.

use crate::ids::{ContentHash, Id};
use crate::nodes::forensic::{AuditEvent, AuditKind};
use crate::time::Timestamp;

/// The version byte prefixing every canonical signing payload.
///
/// Bump this — and the domain-separation tags — whenever the layout changes, so a
/// signature made under one layout can never validate under another. v2 signs ids as
/// their 16 raw UUID bytes (rather than the former 26-char ULID string).
pub const SIGNING_ENCODING_VERSION: u8 = 2;

const PROVENANCE_TAG: &str = "aionforge.provenance.v2";
const ATTESTATION_TAG: &str = "aionforge.attestation.v2";
const CORE_EDIT_ATTESTATION_TAG: &str = "aionforge.core-edit-attestation.v2";
const AUDIT_TAG: &str = "aionforge.audit.v2";

/// The canonical provenance signing payload over `(subject_id, writer_agent_id,
/// ingested_at)` (02 §10).
///
/// The writer signs these bytes; verification recomputes them from the stored
/// `ProvenanceRecord` fields and checks them against the writer's public key.
#[must_use]
pub fn provenance_payload(
    subject_id: &Id,
    writer_agent_id: &Id,
    ingested_at: &Timestamp,
) -> Vec<u8> {
    let subject = subject_id.as_uuid();
    let writer = writer_agent_id.as_uuid();
    encode(
        PROVENANCE_TAG,
        &[subject.as_bytes(), writer.as_bytes()],
        ingested_at,
    )
}

/// The canonical attestation signing payload over `(fact_id, attester_id,
/// attested_at)` (02 §10).
///
/// The attester signs these bytes; verification recomputes them from the stored
/// `ATTESTED_BY` edge fields and checks them against the attester's public key.
#[must_use]
pub fn attestation_payload(fact_id: &Id, attester_id: &Id, attested_at: &Timestamp) -> Vec<u8> {
    let fact = fact_id.as_uuid();
    let attester = attester_id.as_uuid();
    encode(
        ATTESTATION_TAG,
        &[fact.as_bytes(), attester.as_bytes()],
        attested_at,
    )
}

/// The canonical core-block edit attestation payload over `(block_id, attester_id,
/// prior_content_hash, new_content_hash, attested_at)` (05 §4, M5.T04).
///
/// A core block's id is deliberately **stable for the block's life** — unlike a fact,
/// whose content-addressed id binds an attestation to its content all by itself — so a
/// vote signed over the bare id (the fact-shaped [`attestation_payload`]) would
/// authorize *any* content swap of that block inside the clock-skew window. This
/// payload binds the exact transition instead: the attester vouches that the block
/// move from these prior bytes to these new bytes, and nothing else. The hashes go in
/// as their canonical hex strings, length-prefixed like every field.
#[must_use]
pub fn core_edit_attestation_payload(
    block_id: &Id,
    attester_id: &Id,
    prior_content_hash: &ContentHash,
    new_content_hash: &ContentHash,
    attested_at: &Timestamp,
) -> Vec<u8> {
    let block = block_id.as_uuid();
    let attester = attester_id.as_uuid();
    encode(
        CORE_EDIT_ATTESTATION_TAG,
        &[
            block.as_bytes(),
            attester.as_bytes(),
            prior_content_hash.as_str().as_bytes(),
            new_content_hash.as_str().as_bytes(),
        ],
        attested_at,
    )
}

/// The canonical audit signing payload over an [`AuditEvent`]'s authoritative
/// content: `(id, kind, subject_id, actor_id, canonical(payload), occurred_at)`
/// (02 §10, 06 §6).
///
/// The substrate signs these bytes over the events it authors; verification
/// recomputes them from the stored `AuditEvent` and checks them against the
/// substrate's public key. Two fields are deliberately **excluded**:
/// - `signature`, the value being computed — it can never sign over itself; and
/// - `identity.ingested_at`, a store write-clock that a crash recovery re-stamps —
///   signing over it would make the signature un-recomputable. The event's
///   authoritative, immutable instant is `occurred_at`, which is what binds the
///   signature.
///
/// The `payload` JSON is canonicalized (object keys sorted at every depth) so the
/// bytes are stable regardless of map construction order. The whole canonicalized
/// payload goes in as one length-prefixed field, so its bytes can never bleed into
/// an adjacent field.
#[must_use]
pub fn audit_payload(event: &AuditEvent) -> Vec<u8> {
    let id = event.identity.id.as_uuid();
    let subject = event.subject_id.as_uuid();
    let actor = event.actor_id.as_uuid();
    let kind = audit_kind_tag(event.kind);
    let payload = canonical_json(&event.payload);
    encode(
        AUDIT_TAG,
        &[
            id.as_bytes(),
            kind.as_bytes(),
            subject.as_bytes(),
            actor.as_bytes(),
            &payload,
        ],
        &event.occurred_at,
    )
}

/// The canonical `snake_case` spec token for an audit kind — the exact string the
/// store persists (`convert::enum_value`), so the signed kind and the stored kind
/// are byte-identical. A fieldless enum with a `serde(rename_all)` always
/// serializes to a JSON string, so this is infallible.
fn audit_kind_tag(kind: AuditKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|json| json.as_str().map(str::to_owned))
        .expect("an AuditKind serializes to a snake_case string")
}

/// Serialize a JSON value to canonical bytes with object keys sorted
/// lexicographically at every depth.
///
/// This does not lean on `serde_json::Value`'s map backend: `serde_json`'s
/// `preserve_order` feature (which any crate in the build graph can switch on,
/// since cargo unifies features globally) makes `Value::Object` an insertion-ordered
/// `IndexMap` instead of a sorted `BTreeMap`. Sorting here keeps the signed bytes
/// stable either way, while still reusing `serde_json` for scalar, number, and
/// string formatting (which are order-independent).
fn canonical_json(value: &serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&canonicalize(value)).expect("a serde_json::Value re-serializes")
}

/// Recursively rebuild a JSON value with every object's keys in sorted order.
/// Inserting in sorted order yields sorted iteration under both map backends.
fn canonicalize(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
            let mut sorted = serde_json::Map::new();
            for (key, child) in entries {
                sorted.insert(key.clone(), canonicalize(child));
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        scalar => scalar.clone(),
    }
}

/// Encode a versioned, domain-separated, length-prefixed payload: the version
/// byte, then the tag, then each field, then the instant as big-endian epoch
/// milliseconds. Ids arrive as their 16 raw UUID bytes.
fn encode(tag: &str, fields: &[&[u8]], instant: &Timestamp) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.push(SIGNING_ENCODING_VERSION);
    push_field(&mut buf, tag.as_bytes());
    for &field in fields {
        push_field(&mut buf, field);
    }
    let millis = instant.timestamp().as_millisecond();
    buf.extend_from_slice(&millis.to_be_bytes());
    buf
}

/// Append a `u32` big-endian length prefix followed by the bytes, so two adjacent
/// fields can never be reinterpreted as a single field of a different split.
fn push_field(buf: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).expect("signing field length fits in u32");
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::Identity;
    use crate::namespace::Namespace;
    use jiff::Timestamp as Instant;
    use jiff::tz::TimeZone;

    fn ts(ms: i64) -> Timestamp {
        Instant::from_millisecond(ms)
            .unwrap()
            .to_zoned(TimeZone::UTC)
    }

    fn id(seed: u128) -> Id {
        Id::from_uuid(uuid::Uuid::from_u128(seed))
    }

    /// A fully-populated audit event with a distinct id per field, so the difference-based
    /// per-field sweep below catches a *dropped* field. (A *transposed* field — a swap of two
    /// equal-width slots, invisible to a difference test — is pinned by the golden layout's
    /// distinct id/subject/actor values instead.)
    fn audit_event(kind: AuditKind, payload: serde_json::Value) -> AuditEvent {
        AuditEvent {
            identity: Identity {
                id: id(7),
                ingested_at: ts(123),
                namespace: Namespace::System,
                expired_at: None,
            },
            kind,
            subject_id: id(8),
            actor_id: id(9),
            payload,
            signature: String::new(),
            occurred_at: ts(1_700_000_000_000),
        }
    }

    #[test]
    fn payload_is_deterministic() {
        let a = provenance_payload(&id(1), &id(2), &ts(1_700_000_000_000));
        let b = provenance_payload(&id(1), &id(2), &ts(1_700_000_000_000));
        assert_eq!(a, b);
    }

    #[test]
    fn payload_starts_with_the_version_byte() {
        let payload = provenance_payload(&id(1), &id(2), &ts(0));
        assert_eq!(payload[0], SIGNING_ENCODING_VERSION);
    }

    #[test]
    fn distinct_inputs_yield_distinct_payloads() {
        let base = provenance_payload(&id(1), &id(2), &ts(10));
        assert_ne!(base, provenance_payload(&id(9), &id(2), &ts(10)));
        assert_ne!(base, provenance_payload(&id(1), &id(9), &ts(10)));
        assert_ne!(base, provenance_payload(&id(1), &id(2), &ts(11)));
    }

    #[test]
    fn domain_separation_prevents_cross_protocol_reuse() {
        let prov = provenance_payload(&id(1), &id(2), &ts(5));
        let att = attestation_payload(&id(1), &id(2), &ts(5));
        assert_ne!(prov, att);
    }

    #[test]
    fn a_core_edit_vote_signs_the_exact_transition() {
        let prior = ContentHash::of(b"the prior content");
        let new = ContentHash::of(b"the new content");
        let base = core_edit_attestation_payload(&id(1), &id(2), &prior, &new, &ts(5));

        // Deterministic, versioned, and domain-separated from the fact-shaped payload
        // built over the same ids and instant.
        assert_eq!(
            base,
            core_edit_attestation_payload(&id(1), &id(2), &prior, &new, &ts(5))
        );
        assert_eq!(base[0], SIGNING_ENCODING_VERSION);
        assert_ne!(base, attestation_payload(&id(1), &id(2), &ts(5)));

        // Every field is signed — most importantly both content hashes, which is the
        // whole point: a vote for one transition can never validate another.
        let other = ContentHash::of(b"something else");
        assert_ne!(
            base,
            core_edit_attestation_payload(&id(9), &id(2), &prior, &new, &ts(5))
        );
        assert_ne!(
            base,
            core_edit_attestation_payload(&id(1), &id(9), &prior, &new, &ts(5))
        );
        assert_ne!(
            base,
            core_edit_attestation_payload(&id(1), &id(2), &other, &new, &ts(5))
        );
        assert_ne!(
            base,
            core_edit_attestation_payload(&id(1), &id(2), &prior, &other, &ts(5))
        );
        assert_ne!(
            base,
            core_edit_attestation_payload(&id(1), &id(2), &prior, &new, &ts(6))
        );

        // Swapping prior and new is a *different* transition, not the same bytes.
        assert_ne!(
            base,
            core_edit_attestation_payload(&id(1), &id(2), &new, &prior, &ts(5))
        );
    }

    #[test]
    fn length_prefix_prevents_field_boundary_collisions() {
        let split_a = encode("t", &[&b"ab"[..], &b"c"[..]], &ts(0));
        let split_b = encode("t", &[&b"a"[..], &b"bc"[..]], &ts(0));
        assert_ne!(split_a, split_b);
    }

    #[test]
    fn audit_payload_is_deterministic_and_versioned() {
        let event = audit_event(AuditKind::Promote, serde_json::json!({"k": 1}));
        let a = audit_payload(&event);
        let b = audit_payload(&event);
        assert_eq!(a, b);
        assert_eq!(a[0], SIGNING_ENCODING_VERSION);
    }

    /// The reconstructed-golden: an independent, byte-by-byte rebuild of the wire layout
    /// for an all-zero event with an empty payload. Re-derived through a different path than
    /// the implementation (manual concatenation, not `encode`/`push_field`), so it locks the
    /// exact format — any future layout change fails here.
    #[test]
    fn audit_payload_golden_layout() {
        // Distinct id/subject/actor values, each pinned at its own offset below. The
        // difference-based per-field sweep cannot see a *transposition* of two equal-width UUID
        // slots, so the golden carries that part of the contract: a swap of these three fields in
        // `encode`'s array lands the wrong bytes here and fails.
        let event = AuditEvent {
            identity: Identity {
                id: id(1),
                ingested_at: ts(999),
                namespace: Namespace::System,
                expired_at: None,
            },
            kind: AuditKind::Capture,
            subject_id: id(2),
            actor_id: id(3),
            payload: serde_json::json!({}),
            signature: "ignored".to_string(),
            occurred_at: ts(0),
        };

        // The 16-byte big-endian form of `Uuid::from_u128(n)` for a small `n`: all zeros but the
        // final byte. Rebuilt by hand so the golden does not borrow the implementation's id codec.
        let uuid_be = |n: u8| {
            let mut bytes = [0u8; 16];
            bytes[15] = n;
            bytes
        };

        let mut expected = Vec::new();
        expected.push(SIGNING_ENCODING_VERSION);
        expected.extend_from_slice(&18u32.to_be_bytes());
        expected.extend_from_slice(b"aionforge.audit.v2");
        expected.extend_from_slice(&16u32.to_be_bytes());
        expected.extend_from_slice(&uuid_be(1)); // id
        expected.extend_from_slice(&7u32.to_be_bytes());
        expected.extend_from_slice(b"capture");
        expected.extend_from_slice(&16u32.to_be_bytes());
        expected.extend_from_slice(&uuid_be(2)); // subject
        expected.extend_from_slice(&16u32.to_be_bytes());
        expected.extend_from_slice(&uuid_be(3)); // actor
        expected.extend_from_slice(&2u32.to_be_bytes());
        expected.extend_from_slice(b"{}"); // canonical empty payload
        expected.extend_from_slice(&0i64.to_be_bytes()); // occurred_at millis

        assert_eq!(audit_payload(&event), expected);
    }

    #[test]
    fn audit_payload_is_domain_separated_from_other_payloads() {
        let event = audit_event(AuditKind::Attest, serde_json::json!({}));
        let aud = audit_payload(&event);
        // Same ids and instant fed to the sibling payloads — only the tag differs.
        let prov = provenance_payload(&event.identity.id, &event.actor_id, &event.occurred_at);
        let att = attestation_payload(&event.subject_id, &event.actor_id, &event.occurred_at);
        assert_ne!(aud, prov);
        assert_ne!(aud, att);
    }

    #[test]
    fn audit_payload_excludes_signature_and_ingested_at_but_signs_occurred_at() {
        let base = audit_event(AuditKind::Demote, serde_json::json!({"k": "v"}));

        // signature (the value being computed) and ingested_at (a store write-clock that a
        // recovery re-stamps) are not part of the signed bytes.
        let mut noise = base.clone();
        noise.signature = "a-totally-different-signature".to_string();
        noise.identity.ingested_at = ts(42_000);
        assert_eq!(audit_payload(&base), audit_payload(&noise));

        // ...but occurred_at, the authoritative instant, is.
        let mut moved = base.clone();
        moved.occurred_at = ts(1_700_000_001_000);
        assert_ne!(audit_payload(&base), audit_payload(&moved));
    }

    #[test]
    fn audit_payload_distinguishes_each_signed_field() {
        let base = audit_event(AuditKind::Promote, serde_json::json!({"k": 1}));
        let bytes = audit_payload(&base);

        let mut e = base.clone();
        e.identity.id = id(99);
        assert_ne!(bytes, audit_payload(&e), "id is signed");
        let mut e = base.clone();
        e.kind = AuditKind::Demote;
        assert_ne!(bytes, audit_payload(&e), "kind is signed");
        let mut e = base.clone();
        e.subject_id = id(99);
        assert_ne!(bytes, audit_payload(&e), "subject is signed");
        let mut e = base.clone();
        e.actor_id = id(99);
        assert_ne!(bytes, audit_payload(&e), "actor is signed");
        let mut e = base.clone();
        e.payload = serde_json::json!({"k": 2});
        assert_ne!(bytes, audit_payload(&e), "payload is signed");
    }

    /// The canonical payload field is key-sorted at every depth, so the signed bytes do not
    /// depend on construction order. Under serde_json's default sorted-map backend the input is
    /// already ordered; the explicit sort in `canonicalize` is what carries this invariant if a
    /// build elsewhere flips on the `preserve_order` (insertion-order) feature.
    #[test]
    fn audit_payload_sorts_payload_keys_at_every_depth() {
        let event = audit_event(
            AuditKind::Promote,
            serde_json::json!({"b": 1, "a": {"y": 2, "x": 3}}),
        );
        let bytes = audit_payload(&event);
        let needle = br#"{"a":{"x":3,"y":2},"b":1}"#;
        assert!(
            bytes.windows(needle.len()).any(|w| w == needle),
            "payload object keys must be sorted at every depth"
        );
    }

    /// The typed `KeyRotation` payload (PR-5b) signs stably. Its `Timestamp` fields serialize
    /// to RFC 9557 strings inside the payload JSON — strings round-trip exactly, so the same
    /// rotation built twice, read back from the store, or parsed and re-serialized through
    /// `from_value`/`to_value` all sign to identical bytes. The verifier never re-emits — it
    /// checks the signature over the payload as stored — but the emitter builds rotations
    /// through this struct, and these legs pin it as a faithful codec: a crash-replay rebuild
    /// signs the same bytes, so the store's dedup-by-id write stays a true no-op.
    #[test]
    fn a_key_rotation_payload_signs_stably() {
        use crate::nodes::forensic::KeyRotationPayload;
        let admitted: Timestamp = "2026-06-09T10:00:00-05:00[America/Chicago]"
            .parse()
            .expect("valid zoned datetime");
        let rotation = KeyRotationPayload {
            announced_pubkey_b64: "bmV3LWtleQ==".to_string(),
            predecessor_pubkey_b64: Some("b2xkLWtleQ==".to_string()),
            admitted_at: admitted.clone(),
            retired_at: Some(admitted),
        };

        // Same rotation, built twice: identical signed bytes.
        let event = audit_event(AuditKind::KeyRotation, rotation.to_value());
        let twin = audit_event(AuditKind::KeyRotation, rotation.to_value());
        let bytes = audit_payload(&event);
        assert_eq!(bytes, audit_payload(&twin));

        // Read back from the store (JSON string round-trip): identical signed bytes.
        let mut round_tripped = event.clone();
        let as_string = serde_json::to_string(&event.payload).unwrap();
        round_tripped.payload = serde_json::from_str(&as_string).unwrap();
        assert_eq!(bytes, audit_payload(&round_tripped));

        // Parsed into the typed struct and re-serialized: identical signed bytes.
        let mut rebuilt = event.clone();
        rebuilt.payload = KeyRotationPayload::from_value(&event.payload)
            .expect("a stored rotation payload parses")
            .to_value();
        assert_eq!(bytes, audit_payload(&rebuilt));
    }

    /// The bytes survive a JSON round-trip, so a signature stamped at emit time still recomputes
    /// after the event is read back from the store. This drives the payload through a string
    /// (`to_string`/`from_str`) — a strictly harder case than the store's own structural
    /// `Value -> JsonValue -> Value` clone, since it additionally forces the float through textual
    /// reformatting. Guards the float round-trip in particular.
    #[test]
    fn audit_payload_is_stable_across_a_json_round_trip() {
        let event = audit_event(
            AuditKind::Distill,
            serde_json::json!({"z": [1, 2], "a": {"n": 0.772}}),
        );
        let before = audit_payload(&event);

        let mut round_tripped = event.clone();
        let as_string = serde_json::to_string(&event.payload).unwrap();
        round_tripped.payload = serde_json::from_str(&as_string).unwrap();

        assert_eq!(before, audit_payload(&round_tripped));
    }
}
