//! The forensic kinds: signed write proofs, the audit log, and the promotion
//! ledger (02 §4.10–§4.12).
//!
//! These are not retrievable memories, so they carry only the reduced
//! [`Identity`] block (no [`crate::blocks::Stats`]), per 02 §3. They form the
//! substrate's tamper-evident provenance and accountability trail.

use serde::{Deserialize, Serialize};

use crate::blocks::Identity;
use crate::ids::Id;
use crate::time::Timestamp;

/// A signed write proof attesting who wrote a memory and under what trust (02 §4.10).
///
/// One provenance record is emitted per memory write and linked back to the memory
/// via `HAS_PROVENANCE`. The `signature` is a base64 Ed25519 signature over the
/// canonical encoding of the write, making the authorship and write-time trust
/// non-repudiable. Trust here is a snapshot at write time, so `trust_at_write` is a
/// float and the struct derives `PartialEq` only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProvenanceRecord {
    /// Shared identity block.
    pub identity: Identity,
    /// The memory this record proves the provenance of.
    pub subject_id: Id,
    /// The agent that performed the write.
    pub writer_agent_id: Id,
    /// Base64 Ed25519 signature over the canonical encoding (immutable).
    pub signature: String,
    /// The episodes the written memory was derived from.
    pub source_episode_ids: Vec<Id>,
    /// Writer model family.
    pub model_family: String,
    /// Writer model version, if known.
    pub model_version: Option<String>,
    /// The writer/derivation trust captured at write time.
    pub trust_at_write: f64,
}

impl ProvenanceRecord {
    /// The selene-db node label for this kind.
    pub const LABEL: &str = "ProvenanceRecord";
}

/// The kind of audit event recorded (02 §4.11).
///
/// Enumerates every lifecycle, governance, and guard event the substrate audits.
/// Serialized in `snake_case` to match the spec's `kind` vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    /// An episode was captured.
    Capture,
    /// A memory was soft-forgotten (expired).
    Forget,
    /// A memory was hard-purged (erased).
    Purge,
    /// A memory was quarantined pending review.
    Quarantine,
    /// A soft-forgotten memory was restored.
    Unforget,
    /// A memory was pinned: held at full importance and spared from every sweep.
    Pin,
    /// A memory's pin was lifted, re-arming decay and forgetting eligibility.
    Unpin,
    /// A fact or core block was attested.
    Attest,
    /// A candidate was promoted across namespaces.
    Promote,
    /// A memory was demoted (lost support).
    Demote,
    /// A core block was edited.
    CoreEdit,
    /// A skill was saved.
    SkillSave,
    /// A skill was deprecated.
    SkillDeprecate,
    /// A skill version diff was recorded.
    SkillVersionDiff,
    /// Entities/facts were canonicalized.
    Canonicalize,
    /// An episode cluster was summarized into a note (or a summary was skipped to bound
    /// lost detail; the payload's outcome distinguishes the two).
    Summarize,
    /// **Retained-for-decode tombstone — no longer emitted.** This variant recorded the
    /// optional, off-by-default LLM note-distiller (M3.T08), which has since been removed; no
    /// path in the substrate emits a `Distill` event anymore. The variant is **kept** because it
    /// is signed/serialized audit vocabulary that older stores may still hold and that the store's
    /// `distill_models_for` lineage read (along with the erasure-cascade and signing codecs) still
    /// decodes; deleting it would break reading those historical rows. Any new code should treat it
    /// as read-only. A live store under deterministic consolidation simply finds zero of these rows.
    Distill,
    /// Note links were evolved.
    LinkEvolve,
    /// A skill was induced from experience.
    InduceSkill,
    /// An agent reliability score was updated.
    ReliabilityUpdate,
    /// Importance scores were recomputed.
    ImportanceRecompute,
    /// A consolidation pass failed.
    ConsolidationFailed,
    /// The subliminal-learning guard raised a warning.
    SubliminalGuardWarning,
    /// The drift detector found a core block's drift score at or above the threshold
    /// (05 §1, M5.T05) — the T6 twin of
    /// [`SubliminalGuardWarning`](AuditKind::SubliminalGuardWarning). The block is
    /// the row's `subject_id` and the row lives in the block's own namespace; the
    /// payload records the namespace (again, for hosts that read payloads alone),
    /// the block kind, the score and threshold, the baseline instant, the sample
    /// size, and the embedder identity the comparison ran under. The row doubles as
    /// the notification outbox a host pages with `audit_by_kind`.
    DriftWarning,
    /// A fact proximate to a high-trust core block was stamped with a cooling window
    /// (05 §1, M5.T05): admitted, but rank-time trust is reduced until the stamp
    /// expires, giving the drift detector time to flag sycophantic drift first.
    Cooled,
    /// A write was rejected by namespace authorization: the agent is not permitted to write the
    /// target namespace (06 §1). The payload records the agent, the requested namespace, and the
    /// deny reason — the audit of a cross-namespace write attempt (07 §T9).
    NamespaceDenied,
    /// A write was rejected for clock skew.
    ClockSkewRejected,
    /// A capture was rejected because injection-marker excision left only residue (07 §5):
    /// the cleaned content retained no substance worth remembering. The payload records the
    /// agent, the markers that fired, and the original/cleaned lengths — not the residue
    /// text itself, so the audit log never re-hosts fragments of a filtered injection.
    ResidueRejected,
    /// A capture was rejected because its supersedes hint named no live episode the writer
    /// may supersede (04 §1 step 3). The payload records the claimed target id and the
    /// specific cause (not found vs. not writable) for forensics, while the returned error
    /// stays coarse so the hint is no existence oracle.
    SupersedesRejected,
    /// A signature failed verification.
    InvalidSignature,
    /// A substrate audit-signing key entered service (genesis or rotation). The payload is
    /// the typed [`KeyRotationPayload`] (06 §6).
    KeyRotation,
    /// An agent was retired.
    AgentRetired,
    /// A work item's `work_status` was advanced (work-structure design §2). The payload records
    /// the `from`/`to` states; the event's `subject_id` is the work item and its `occurred_at` is
    /// when the transition happened, so a work item's lifecycle is the by-subject audit history.
    WorkStatusChange,
}

/// A single forensic audit record — the highest-cardinality kind (02 §4.11).
///
/// Every consequential substrate action emits an audit event signed by the
/// substrate keypair, linked to its subject via the `AUDIT` edge. The `payload`
/// is an intentionally open, kind-specific JSON shape (see [`AuditEvent::payload`]
/// and 02 §6.4). Because `payload` is a [`serde_json::Value`] and `occurred_at` is
/// a [`Timestamp`], the struct derives `PartialEq` only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Shared identity block.
    pub identity: Identity,
    /// The kind of event.
    pub kind: AuditKind,
    /// The memory or node the event is about.
    pub subject_id: Id,
    /// The actor (agent/substrate) that performed the action.
    pub actor_id: Id,
    /// Kind-specific structured detail; an intentionally open shape (02 §6.4).
    pub payload: serde_json::Value,
    /// Substrate-keypair signature over the canonical encoding (immutable).
    pub signature: String,
    /// Event time: when the action occurred (immutable).
    pub occurred_at: Timestamp,
}

impl AuditEvent {
    /// The selene-db node label for this kind.
    pub const LABEL: &str = "AuditEvent";
}

/// The typed payload of a [`AuditKind::KeyRotation`] audit event (06 §6): a substrate
/// audit-signing key entering service.
///
/// Two shapes share this schema:
/// - **Genesis** — the substrate's first key. `predecessor_pubkey_b64` and `retired_at` are
///   `None`; the event is signed by the announced key itself (the only self-signed rotation),
///   and `admitted_at` doubles as the instant the substrate first had a key at all.
/// - **Rotation** — a successor key. The event is signed by the *predecessor* (a key that is
///   already trusted), announces the new key, and stamps `retired_at` — the upper bound of the
///   predecessor's validity window, so a leaked old seed cannot keep signing events that read
///   as valid forever.
///
/// A key's validity window is `[its admitted_at, the retired_at announced by its successor)`;
/// the genesis window stays open until the first rotation closes it.
///
/// Trust never bootstraps from this event. The anchor is the substrate's out-of-band keyring
/// file; the in-band `KeyRotation` event is that file's verifiable echo in the audit trail —
/// a rotation whose announcing event is not signed by an already-trusted key, or whose key is
/// absent from the keyring file, enrolls nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyRotationPayload {
    /// The base64 Ed25519 public key entering service.
    pub announced_pubkey_b64: String,
    /// The base64 public key being rotated out; `None` on the genesis rotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor_pubkey_b64: Option<String>,
    /// When the announced key entered service (its validity-window lower bound).
    pub admitted_at: Timestamp,
    /// When the predecessor stopped being valid for new events; `None` on genesis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<Timestamp>,
}

impl KeyRotationPayload {
    /// The JSON value carried as the event's [`AuditEvent::payload`]. Absent options are
    /// omitted (not `null`), so a genesis payload is terse and the canonical signed bytes
    /// carry no inert keys.
    #[must_use]
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self)
            .expect("a KeyRotationPayload of strings and timestamps serializes to JSON")
    }

    /// Parse a payload back from a stored [`AuditEvent::payload`].
    ///
    /// Unknown fields are tolerated, so a later schema revision can add a field without
    /// breaking this parser. That leniency is safe because integrity comes from the
    /// signature over the canonical payload bytes, not from field rejection — an added or
    /// altered field changes the signed bytes and fails verification on its own.
    ///
    /// # Errors
    /// Returns the deserialization error when a required field is missing or mistyped. One
    /// failure mode involves no tampering at all: [`Timestamp`] parsing rejects a stored
    /// offset that the host's current tz database no longer agrees with for that zone and
    /// instant (a tzdb revision between emit and parse). A parse failure is therefore not a
    /// tamper signal — integrity rides on the signature over the stored bytes — and emitters
    /// sidestep the conflict entirely by stamping the window timestamps in UTC.
    pub fn from_value(value: &serde_json::Value) -> Result<Self, serde_json::Error> {
        use serde::Deserialize as _;
        Self::deserialize(value)
    }
}

/// The resolution status of a promotion candidate (02 §4.12).
///
/// Serialized in `snake_case`. [`Default`] is [`PromotionStatus::Pending`], the state a
/// candidate sits in before quorum is reached. The substrate writes the ledger row only on a
/// terminal transition (`promoted` or `rejected`), each with an explicit status — there is no
/// DB `DEFAULT`, so a row is never persisted with an implicit one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromotionStatus {
    /// Awaiting the attestation quorum.
    #[default]
    Pending,
    /// Quorum reached; the candidate was promoted.
    Promoted,
    /// Quorum failed or the candidate was contradicted.
    Rejected,
}

/// A cross-namespace promotion ledger entry (02 §4.12).
///
/// Records a fact's progress toward quorum-gated promotion (team → global): the
/// posterior probability, the number of attestations collected, and the eventual
/// resolution. `posterior` is a float, so the struct derives `PartialEq` only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Promotion {
    /// Shared identity block.
    pub identity: Identity,
    /// The fact being considered for promotion.
    pub candidate_fact_id: Id,
    /// The posterior probability the candidate is correct.
    pub posterior: f64,
    /// The number of attestations collected so far.
    pub k: u64,
    /// The current resolution status.
    pub status: PromotionStatus,
    /// When the candidate was resolved; `None` while pending.
    pub resolved_at: Option<Timestamp>,
    /// The promoted fact produced on success; `None` until promoted.
    pub promoted_fact_id: Option<Id>,
}

impl Promotion {
    /// The selene-db node label for this kind.
    pub const LABEL: &str = "Promotion";
}

#[cfg(test)]
mod tests {
    use super::KeyRotationPayload;
    use crate::time::Timestamp;

    fn at(s: &str) -> Timestamp {
        s.parse().expect("valid zoned datetime")
    }

    fn genesis() -> KeyRotationPayload {
        KeyRotationPayload {
            announced_pubkey_b64: "Z2VuZXNpcy1rZXk=".to_owned(),
            predecessor_pubkey_b64: None,
            admitted_at: at("2026-06-09T09:00:00-05:00[America/Chicago]"),
            retired_at: None,
        }
    }

    #[test]
    fn a_rotation_payload_round_trips_with_every_field() {
        let rotation = KeyRotationPayload {
            announced_pubkey_b64: "bmV3LWtleQ==".to_owned(),
            predecessor_pubkey_b64: Some("b2xkLWtleQ==".to_owned()),
            admitted_at: at("2026-06-09T10:00:00-05:00[America/Chicago]"),
            retired_at: Some(at("2026-06-09T10:00:00-05:00[America/Chicago]")),
        };
        let value = rotation.to_value();
        let parsed = KeyRotationPayload::from_value(&value).expect("parses back");
        assert_eq!(parsed, rotation);
        // The wire form keeps the full zoned instant (RFC 9557, zone bracket included), so the
        // store's JSON round-trip is exact, not a lossy instant.
        let admitted = value["admitted_at"].as_str().expect("a string timestamp");
        assert!(
            admitted.contains("[America/Chicago]"),
            "kept the zone: {admitted}"
        );
    }

    #[test]
    fn a_genesis_payload_omits_its_absent_fields_and_round_trips() {
        let value = genesis().to_value();
        let object = value.as_object().expect("an object payload");
        assert!(
            !object.contains_key("predecessor_pubkey_b64") && !object.contains_key("retired_at"),
            "absent options are omitted, not null: {value}"
        );
        let parsed = KeyRotationPayload::from_value(&value).expect("parses back");
        assert_eq!(parsed, genesis());
    }

    #[test]
    fn unknown_payload_fields_are_tolerated() {
        // Forward-compat: a later schema revision may add a field. Integrity is anchored by
        // the signature over the canonical bytes, so leniency here smuggles nothing.
        let mut value = genesis().to_value();
        value
            .as_object_mut()
            .expect("an object payload")
            .insert("future_field".to_owned(), serde_json::json!(1));
        let parsed = KeyRotationPayload::from_value(&value).expect("tolerates the unknown field");
        assert_eq!(parsed, genesis());
    }

    #[test]
    fn a_payload_missing_a_required_field_is_rejected() {
        // Both non-optional fields: the announced key and the window lower bound. Pinning
        // each keeps a refactor from quietly loosening one to an `Option` "for symmetry".
        for required in ["announced_pubkey_b64", "admitted_at"] {
            let mut value = genesis().to_value();
            value
                .as_object_mut()
                .expect("an object payload")
                .remove(required);
            assert!(
                KeyRotationPayload::from_value(&value).is_err(),
                "a rotation without {required} is malformed"
            );
        }
    }
}
