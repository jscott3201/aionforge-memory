//! Shared proptest strategies and the round-trip helper for the domain
//! serialization tests.
//!
//! Lives under `tests/support/` so cargo does not compile it as its own test
//! binary; it is included via `mod support;` from the test files. Holds the leaf,
//! value, and enum strategies plus the generic `round_trip` assertion; the per-kind
//! node strategies live alongside the tests that use them.

#![allow(dead_code)]

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::edges::EdgeLabel;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::agent::{AgentStatus, TrustCategory, TrustScores};
use aionforge_domain::nodes::core::BlockKind;
use aionforge_domain::nodes::episodic::{ConsolidationState, Origin, Redaction, Role};
use aionforge_domain::nodes::forensic::{AuditKind, PromotionStatus};
use aionforge_domain::nodes::semantic::{Extraction, FactStatus, SourceSpan};
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::value::ObjectValue;
use proptest::prelude::*;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Serialize a value to JSON, deserialize it back, and assert exact equality.
pub fn round_trip<T>(value: T)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(&value).expect("serialize");
    let back: T = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(value, back, "round-trip mismatch via JSON: {json}");
}

// ---- leaf and value strategies ----

pub fn arb_id() -> impl Strategy<Value = Id> {
    any::<[u8; 16]>().prop_map(|bytes| Id::from_uuid(uuid::Uuid::from_bytes(bytes)))
}

pub fn arb_content_hash() -> impl Strategy<Value = ContentHash> {
    prop::collection::vec(any::<u8>(), 0..24).prop_map(|bytes| ContentHash::of(&bytes))
}

pub fn arb_timestamp() -> impl Strategy<Value = Timestamp> {
    (0i64..=4_000_000_000i64, 0i32..1_000_000_000i32).prop_map(|(secs, nanos)| {
        jiff::Timestamp::new(secs, nanos)
            .expect("valid timestamp")
            .to_zoned(jiff::tz::TimeZone::UTC)
    })
}

pub fn arb_opt_ts() -> impl Strategy<Value = Option<Timestamp>> {
    prop::option::of(arb_timestamp())
}

pub fn arb_f64() -> impl Strategy<Value = f64> {
    proptest::num::f64::ANY.prop_filter("finite", |x| x.is_finite())
}

pub fn arb_namespace() -> impl Strategy<Value = Namespace> {
    prop_oneof![
        "[a-z0-9]{1,12}".prop_map(Namespace::Agent),
        "[a-z0-9]{1,12}".prop_map(Namespace::Team),
        Just(Namespace::Global),
        Just(Namespace::System),
    ]
}

pub fn arb_embedding() -> impl Strategy<Value = Embedding> {
    prop::collection::vec(
        proptest::num::f32::ANY.prop_filter("finite", |x| x.is_finite()),
        1..16,
    )
    .prop_map(|components| Embedding::new(components).expect("finite, non-empty"))
}

pub fn arb_embedder_model() -> impl Strategy<Value = EmbedderModel> {
    ("[a-z]{1,8}", "[a-z0-9.]{1,8}", any::<u32>()).prop_map(|(family, version, dimension)| {
        EmbedderModel {
            family,
            version,
            dimension,
        }
    })
}

pub fn arb_json() -> impl Strategy<Value = serde_json::Value> {
    let leaf = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::Bool),
        any::<i64>().prop_map(|n| serde_json::Value::Number(n.into())),
        arb_f64().prop_map(|f| serde_json::Number::from_f64(f)
            .map_or(serde_json::Value::Null, serde_json::Value::Number)),
        any::<String>().prop_map(serde_json::Value::String),
    ];
    leaf.prop_recursive(3, 24, 5, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..5).prop_map(serde_json::Value::Array),
            prop::collection::btree_map("[a-z]{1,6}", inner, 0..5)
                .prop_map(|map| serde_json::Value::Object(map.into_iter().collect())),
        ]
    })
}

/// A nullable JSON field strategy that excludes the degenerate `Some(Value::Null)`.
///
/// For a nullable JSON column, `None` (property absent) and `Some(Value::Null)`
/// (property present, holding the JSON literal `null`) are semantically identical —
/// both mean "no value" — and `None` is the canonical form. They are also
/// indistinguishable after a `serde` round-trip: `Some(Null)` serializes to bare
/// `null`, which deserializes back to `None`. Generating only `None` or
/// `Some(non-null)` keeps the round-trip exact without testing a state the model
/// does not need to represent distinctly. (Nested nulls — e.g. `{"a": null}` — are
/// still exercised via [`arb_json`], since only a top-level null collapses.)
pub fn arb_opt_json() -> impl Strategy<Value = Option<serde_json::Value>> {
    prop::option::of(arb_json().prop_filter("not a top-level JSON null", |v| !v.is_null()))
}

pub fn arb_identity() -> impl Strategy<Value = Identity> {
    (arb_id(), arb_timestamp(), arb_namespace(), arb_opt_ts()).prop_map(
        |(id, ingested_at, namespace, expired_at)| Identity {
            id,
            ingested_at,
            namespace,
            expired_at,
        },
    )
}

pub fn arb_stats() -> impl Strategy<Value = Stats> {
    (
        arb_f64(),
        arb_f64(),
        arb_timestamp(),
        any::<u64>(),
        any::<u64>(),
        arb_f64(),
        any::<bool>(),
    )
        .prop_map(
            |(
                importance,
                trust,
                last_access,
                access_count_recent,
                referenced_count,
                surprise,
                is_pinned,
            )| Stats {
                importance,
                trust,
                last_access,
                access_count_recent,
                referenced_count,
                surprise,
                is_pinned,
            },
        )
}

pub fn arb_bitemporal() -> impl Strategy<Value = BiTemporal> {
    (arb_timestamp(), arb_opt_ts(), arb_timestamp(), arb_opt_ts()).prop_map(
        |(valid_from, valid_to, ingested_at, expired_at)| BiTemporal {
            valid_from,
            valid_to,
            ingested_at,
            expired_at,
        },
    )
}

pub fn arb_object_value() -> impl Strategy<Value = ObjectValue> {
    prop_oneof![
        arb_id().prop_map(ObjectValue::Entity),
        any::<String>().prop_map(ObjectValue::Text),
        arb_f64().prop_map(ObjectValue::Number),
        any::<bool>().prop_map(ObjectValue::Bool),
        arb_timestamp().prop_map(ObjectValue::DateTime),
        arb_json().prop_map(ObjectValue::Json),
    ]
}

pub fn arb_redaction() -> impl Strategy<Value = Redaction> {
    (
        any::<String>(),
        (any::<usize>(), any::<usize>()),
        any::<String>(),
    )
        .prop_map(|(pattern_id, span, kind)| Redaction {
            pattern_id,
            span,
            kind,
        })
}

pub fn arb_origin() -> impl Strategy<Value = Origin> {
    (
        prop::option::of(any::<String>()),
        prop::option::of(any::<String>()),
        prop::option::of(any::<String>()),
        prop::option::of(any::<String>()),
        prop::collection::vec(arb_redaction(), 0..3),
        prop::collection::vec(any::<String>(), 0..3),
        prop::option::of(any::<u64>()),
        prop::option::of(arb_id()),
    )
        .prop_map(
            |(
                model_family,
                model_version,
                transport,
                request_id,
                redactions,
                injection_flags,
                capture_latency_ms,
                supersedes,
            )| Origin {
                model_family,
                model_version,
                transport,
                request_id,
                redactions,
                injection_flags,
                capture_latency_ms,
                supersedes,
            },
        )
}

pub fn arb_source_span() -> impl Strategy<Value = SourceSpan> {
    (arb_id(), any::<usize>(), any::<usize>()).prop_map(|(episode_id, start, end)| SourceSpan {
        episode_id,
        start,
        end,
    })
}

pub fn arb_extraction() -> impl Strategy<Value = Extraction> {
    (
        prop::option::of(any::<String>()),
        prop::option::of(any::<String>()),
        prop::collection::vec(arb_source_span(), 0..3),
        prop::option::of(any::<String>()),
    )
        .prop_map(
            |(
                extractor_model_family,
                extractor_model_version,
                source_spans,
                extraction_rule_version,
            )| Extraction {
                extractor_model_family,
                extractor_model_version,
                source_spans,
                extraction_rule_version,
            },
        )
}

pub fn arb_trust_scores() -> impl Strategy<Value = TrustScores> {
    let category = (arb_f64(), arb_f64(), arb_f64())
        .prop_map(|(alpha, beta, score)| TrustCategory { alpha, beta, score });
    prop::collection::btree_map("[a-z]{1,6}", category, 0..4).prop_map(TrustScores)
}

// ---- enum strategies ----

pub fn arb_role() -> impl Strategy<Value = Role> {
    prop::sample::select(vec![
        Role::User,
        Role::Assistant,
        Role::Tool,
        Role::System,
        Role::Event,
    ])
}

pub fn arb_consolidation_state() -> impl Strategy<Value = ConsolidationState> {
    prop::sample::select(vec![
        ConsolidationState::Raw,
        ConsolidationState::InProgress,
        ConsolidationState::Consolidated,
        ConsolidationState::Failed,
    ])
}

pub fn arb_fact_status() -> impl Strategy<Value = FactStatus> {
    prop::sample::select(vec![
        FactStatus::Active,
        FactStatus::Quarantined,
        FactStatus::Superseded,
    ])
}

pub fn arb_block_kind() -> impl Strategy<Value = BlockKind> {
    prop::sample::select(vec![
        BlockKind::Persona,
        BlockKind::Commitment,
        BlockKind::Redline,
    ])
}

pub fn arb_agent_status() -> impl Strategy<Value = AgentStatus> {
    prop::sample::select(vec![AgentStatus::Active, AgentStatus::Retired])
}

pub fn arb_promotion_status() -> impl Strategy<Value = PromotionStatus> {
    prop::sample::select(vec![
        PromotionStatus::Pending,
        PromotionStatus::Promoted,
        PromotionStatus::Rejected,
    ])
}

pub fn arb_audit_kind() -> impl Strategy<Value = AuditKind> {
    prop::sample::select(vec![
        AuditKind::Capture,
        AuditKind::Forget,
        AuditKind::Purge,
        AuditKind::Quarantine,
        AuditKind::Unforget,
        AuditKind::Attest,
        AuditKind::Promote,
        AuditKind::Demote,
        AuditKind::CoreEdit,
        AuditKind::SkillSave,
        AuditKind::SkillDeprecate,
        AuditKind::SkillVersionDiff,
        AuditKind::Canonicalize,
        AuditKind::LinkEvolve,
        AuditKind::InduceSkill,
        AuditKind::ReliabilityUpdate,
        AuditKind::ImportanceRecompute,
        AuditKind::ConsolidationFailed,
        AuditKind::SubliminalGuardWarning,
        AuditKind::ClockSkewRejected,
        AuditKind::InvalidSignature,
        AuditKind::KeyRotation,
        AuditKind::AgentRetired,
        AuditKind::WorkStatusChange,
    ])
}

pub fn arb_edge_label() -> impl Strategy<Value = EdgeLabel> {
    prop::sample::select(vec![
        EdgeLabel::Mentions,
        EdgeLabel::About,
        EdgeLabel::Supports,
        EdgeLabel::SupersededBy,
        EdgeLabel::Contradicts,
        EdgeLabel::ValidAt,
        EdgeLabel::InScope,
        EdgeLabel::InSession,
        EdgeLabel::RecentIn,
        EdgeLabel::DependsOn,
        EdgeLabel::DerivedFrom,
        EdgeLabel::AttestedBy,
        EdgeLabel::PromotedTo,
        EdgeLabel::DemotedFrom,
        EdgeLabel::HasFailure,
        EdgeLabel::RelatesTo,
        EdgeLabel::HasProvenance,
        EdgeLabel::Audit,
        EdgeLabel::HasTag,
    ])
}
