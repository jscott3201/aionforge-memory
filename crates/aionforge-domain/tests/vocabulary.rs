//! Closed-vocabulary serialization fidelity tests (02 §4, §5, §11).
//!
//! Every enum that maps to a spec string vocabulary must serialize to the EXACT
//! spec string. A symmetric round-trip test cannot catch a wrong-but-consistent
//! tag (e.g. `"text"` where the spec says `"string"`), so these assertions pin
//! each variant against its literal spec value. This is the regression lock for
//! the object-kind vocabulary defect found in the T02 conformance review.

use aionforge_domain::edges::EdgeLabel;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::agent::AgentStatus;
use aionforge_domain::nodes::core::BlockKind;
use aionforge_domain::nodes::episodic::{ConsolidationState, Role};
use aionforge_domain::nodes::forensic::{AuditKind, PromotionStatus};
use aionforge_domain::nodes::semantic::FactStatus;
use aionforge_domain::value::{ObjectKind, ObjectValue};
use serde::Serialize;

fn tag<T: Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .expect("serialize")
        .as_str()
        .expect("vocabulary enum serializes to a JSON string")
        .to_string()
}

#[test]
fn role_vocabulary() {
    assert_eq!(tag(Role::User), "user");
    assert_eq!(tag(Role::Assistant), "assistant");
    assert_eq!(tag(Role::Tool), "tool");
    assert_eq!(tag(Role::System), "system");
    assert_eq!(tag(Role::Event), "event");
}

#[test]
fn consolidation_state_vocabulary() {
    assert_eq!(tag(ConsolidationState::Raw), "raw");
    assert_eq!(tag(ConsolidationState::InProgress), "in_progress");
    assert_eq!(tag(ConsolidationState::Consolidated), "consolidated");
    assert_eq!(tag(ConsolidationState::Failed), "failed");
}

#[test]
fn fact_status_vocabulary() {
    assert_eq!(tag(FactStatus::Active), "active");
    assert_eq!(tag(FactStatus::Quarantined), "quarantined");
    assert_eq!(tag(FactStatus::Superseded), "superseded");
}

#[test]
fn object_kind_vocabulary() {
    // §4.2: entity / string / number / bool / datetime / json
    assert_eq!(tag(ObjectKind::Entity), "entity");
    assert_eq!(tag(ObjectKind::Text), "string");
    assert_eq!(tag(ObjectKind::Number), "number");
    assert_eq!(tag(ObjectKind::Bool), "bool");
    assert_eq!(tag(ObjectKind::DateTime), "datetime");
    assert_eq!(tag(ObjectKind::Json), "json");
}

#[test]
fn object_value_kind_tags_match_object_kind() {
    // The adjacently-tagged ObjectValue `kind` discriminant must use the same
    // spec vocabulary as ObjectKind.
    let kind_of = |value: ObjectValue| {
        serde_json::to_value(value).expect("serialize")["kind"]
            .as_str()
            .expect("kind is a string")
            .to_string()
    };
    use aionforge_domain::ids::Id;
    assert_eq!(kind_of(ObjectValue::Text("x".to_string())), "string");
    assert_eq!(kind_of(ObjectValue::Number(1.0)), "number");
    assert_eq!(kind_of(ObjectValue::Bool(true)), "bool");
    assert_eq!(kind_of(ObjectValue::Entity(Id::generate())), "entity");
}

#[test]
fn block_kind_vocabulary() {
    assert_eq!(tag(BlockKind::Persona), "persona");
    assert_eq!(tag(BlockKind::Commitment), "commitment");
    assert_eq!(tag(BlockKind::Redline), "redline");
}

#[test]
fn agent_status_vocabulary() {
    assert_eq!(tag(AgentStatus::Active), "active");
    assert_eq!(tag(AgentStatus::Retired), "retired");
}

#[test]
fn promotion_status_vocabulary() {
    assert_eq!(tag(PromotionStatus::Pending), "pending");
    assert_eq!(tag(PromotionStatus::Promoted), "promoted");
    assert_eq!(tag(PromotionStatus::Rejected), "rejected");
}

#[test]
fn audit_kind_vocabulary() {
    // §4.11: the 23 audit-event kinds, in spec order.
    let expected = [
        (AuditKind::Capture, "capture"),
        (AuditKind::Forget, "forget"),
        (AuditKind::Purge, "purge"),
        (AuditKind::Quarantine, "quarantine"),
        (AuditKind::Unforget, "unforget"),
        (AuditKind::Attest, "attest"),
        (AuditKind::Promote, "promote"),
        (AuditKind::Demote, "demote"),
        (AuditKind::CoreEdit, "core_edit"),
        (AuditKind::SkillSave, "skill_save"),
        (AuditKind::SkillDeprecate, "skill_deprecate"),
        (AuditKind::SkillVersionDiff, "skill_version_diff"),
        (AuditKind::Canonicalize, "canonicalize"),
        (AuditKind::LinkEvolve, "link_evolve"),
        (AuditKind::InduceSkill, "induce_skill"),
        (AuditKind::ReliabilityUpdate, "reliability_update"),
        (AuditKind::ImportanceRecompute, "importance_recompute"),
        (AuditKind::ConsolidationFailed, "consolidation_failed"),
        (
            AuditKind::SubliminalGuardWarning,
            "subliminal_guard_warning",
        ),
        (AuditKind::ClockSkewRejected, "clock_skew_rejected"),
        (AuditKind::InvalidSignature, "invalid_signature"),
        (AuditKind::KeyRotation, "key_rotation"),
        (AuditKind::AgentRetired, "agent_retired"),
    ];
    assert_eq!(expected.len(), 23);
    for (variant, want) in expected {
        assert_eq!(tag(variant), want);
    }
}

#[test]
fn edge_label_vocabulary() {
    // §5: the 18 relationship names, SCREAMING_SNAKE_CASE.
    let expected = [
        (EdgeLabel::Mentions, "MENTIONS"),
        (EdgeLabel::About, "ABOUT"),
        (EdgeLabel::Supports, "SUPPORTS"),
        (EdgeLabel::SupersededBy, "SUPERSEDED_BY"),
        (EdgeLabel::Contradicts, "CONTRADICTS"),
        (EdgeLabel::ValidAt, "VALID_AT"),
        (EdgeLabel::InScope, "IN_SCOPE"),
        (EdgeLabel::InSession, "IN_SESSION"),
        (EdgeLabel::RecentIn, "RECENT_IN"),
        (EdgeLabel::DependsOn, "DEPENDS_ON"),
        (EdgeLabel::DerivedFrom, "DERIVED_FROM"),
        (EdgeLabel::AttestedBy, "ATTESTED_BY"),
        (EdgeLabel::PromotedTo, "PROMOTED_TO"),
        (EdgeLabel::DemotedFrom, "DEMOTED_FROM"),
        (EdgeLabel::HasFailure, "HAS_FAILURE"),
        (EdgeLabel::RelatesTo, "RELATES_TO"),
        (EdgeLabel::HasProvenance, "HAS_PROVENANCE"),
        (EdgeLabel::Audit, "AUDIT"),
    ];
    assert_eq!(expected.len(), 18);
    for (variant, want) in expected {
        assert_eq!(tag(variant), want);
        // Each variant's serialized form matches its struct LABEL via as_str().
        assert_eq!(variant.as_str(), want);
    }
}

#[test]
fn namespace_vocabulary() {
    // §11: agent:<id> / team:<id> / global / system.
    assert_eq!(tag(Namespace::Agent("a1".to_string())), "agent:a1");
    assert_eq!(tag(Namespace::Team("t1".to_string())), "team:t1");
    assert_eq!(tag(Namespace::Global), "global");
    assert_eq!(tag(Namespace::System), "system");
}
