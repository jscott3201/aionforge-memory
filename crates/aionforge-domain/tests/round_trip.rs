//! Property-based serialization round-trip tests for every domain kind (02 §14).
//!
//! Asserts that every node kind, edge kind, and structured value serializes and
//! deserializes through `serde_json` without loss — including `ZONED DATETIME`
//! nanosecond precision, `VECTOR` (`f32`) component fidelity, the adjacently-tagged
//! [`ObjectValue`], and the namespace string form. Strategies generate only values
//! the domain types admit (finite floats, non-empty embeddings, valid UUIDs, JSON
//! with finite numbers), so a round-trip failure is a real serialization defect.
//!
//! The leaf, value, and enum strategies plus the `round_trip` helper live in
//! `tests/support`; the per-kind node strategies and the tests live here.

mod support;

use aionforge_domain::edges::{
    About, AttestedBy, Audit, Contradicts, DemotedFrom, DependsOn, DerivedFrom, HasFailure,
    HasProvenance, InScope, InSession, Mentions, PromotedTo, RecentIn, RelatesTo, SupersededBy,
    Supports, ValidAt,
};
use aionforge_domain::nodes::agent::{Agent, Session};
use aionforge_domain::nodes::anchors::{RecencyWindow, Scope, ValidityAnchor};
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::control::{ConsolidationCursor, SchemaVersion};
use aionforge_domain::nodes::core::CoreBlock;
use aionforge_domain::nodes::episodic::Episode;
use aionforge_domain::nodes::forensic::{AuditEvent, Promotion, ProvenanceRecord};
use aionforge_domain::nodes::procedural::{BadPattern, Skill};
use aionforge_domain::nodes::semantic::{Entity, Fact};
use proptest::prelude::*;
use support::*;

// ---- node strategies ----

fn arb_episode() -> impl Strategy<Value = Episode> {
    (
        (
            arb_identity(),
            arb_stats(),
            any::<String>(),
            arb_role(),
            arb_timestamp(),
            arb_id(),
        ),
        (
            prop::option::of(arb_id()),
            arb_content_hash(),
            prop::option::of(arb_embedding()),
            prop::option::of(arb_embedder_model()),
            arb_consolidation_state(),
            prop::option::of(arb_origin()),
        ),
    )
        .prop_map(
            |(
                (identity, stats, content, role, captured_at, agent_id),
                (session_id, content_hash, embedding, embedder_model, consolidation_state, origin),
            )| Episode {
                identity,
                stats,
                content,
                role,
                captured_at,
                agent_id,
                session_id,
                content_hash,
                embedding,
                embedder_model,
                consolidation_state,
                origin,
            },
        )
}

fn arb_fact() -> impl Strategy<Value = Fact> {
    (
        (
            arb_identity(),
            arb_stats(),
            arb_id(),
            any::<String>(),
            arb_object_value(),
            arb_f64(),
        ),
        (
            arb_fact_status(),
            any::<String>(),
            prop::option::of(arb_embedding()),
            prop::option::of(arb_embedder_model()),
            prop::option::of(arb_extraction()),
            prop::option::of(arb_timestamp()),
        ),
    )
        .prop_map(
            |(
                (identity, stats, subject_id, predicate, object, confidence),
                (status, statement, embedding, embedder_model, extraction, cooled_until),
            )| Fact {
                identity,
                stats,
                subject_id,
                predicate,
                object,
                confidence,
                status,
                statement,
                embedding,
                embedder_model,
                extraction,
                cooled_until,
            },
        )
}

fn arb_entity() -> impl Strategy<Value = Entity> {
    (
        arb_identity(),
        arb_stats(),
        any::<String>(),
        any::<String>(),
        prop::collection::vec(any::<String>(), 0..4),
        prop::option::of(any::<String>()),
        prop::option::of(arb_embedding()),
        prop::option::of(arb_embedder_model()),
        arb_opt_json(),
    )
        .prop_map(
            |(
                identity,
                stats,
                canonical_name,
                entity_type,
                aliases,
                description,
                embedding,
                embedder_model,
                attributes,
            )| Entity {
                identity,
                stats,
                canonical_name,
                entity_type,
                aliases,
                description,
                embedding,
                embedder_model,
                attributes,
            },
        )
}

fn arb_skill() -> impl Strategy<Value = Skill> {
    (
        (
            arb_identity(),
            arb_stats(),
            any::<String>(),
            any::<i64>(),
            any::<String>(),
            prop::option::of(arb_embedding()),
            prop::option::of(arb_embedder_model()),
        ),
        (
            any::<String>(),
            any::<String>(),
            arb_json(),
            arb_opt_json(),
            arb_opt_json(),
            prop::collection::vec(any::<String>(), 0..3),
            any::<u64>(),
        ),
        (
            any::<u64>(),
            prop::option::of(arb_f64()),
            arb_content_hash(),
            arb_opt_ts(),
            arb_opt_ts(),
            arb_opt_ts(),
            any::<bool>(),
        ),
    )
        .prop_map(
            |(
                (identity, stats, name, version, description, problem_embedding, embedder_model),
                (
                    language,
                    body,
                    params,
                    preconditions,
                    postconditions,
                    capabilities,
                    success_count,
                ),
                (
                    failure_count,
                    mean_latency_ms,
                    source_hash,
                    last_success_at,
                    last_failure_at,
                    deprecated_at,
                    induced,
                ),
            )| Skill {
                identity,
                stats,
                name,
                version,
                description,
                problem_embedding,
                embedder_model,
                language,
                body,
                params,
                preconditions,
                postconditions,
                capabilities,
                success_count,
                failure_count,
                mean_latency_ms,
                source_hash,
                last_success_at,
                last_failure_at,
                deprecated_at,
                induced,
            },
        )
}

fn arb_bad_pattern() -> impl Strategy<Value = BadPattern> {
    (
        arb_identity(),
        arb_stats(),
        any::<String>(),
        prop::option::of(arb_embedding()),
        prop::option::of(arb_embedder_model()),
        arb_timestamp(),
    )
        .prop_map(
            |(identity, stats, description, embedding, embedder_model, observed_at)| BadPattern {
                identity,
                stats,
                description,
                embedding,
                embedder_model,
                observed_at,
            },
        )
}

fn arb_note() -> impl Strategy<Value = Note> {
    (
        arb_identity(),
        arb_stats(),
        any::<String>(),
        prop::option::of(any::<String>()),
        prop::collection::vec(any::<String>(), 0..4),
        prop::option::of(arb_embedding()),
        prop::option::of(arb_embedder_model()),
        prop::option::of(arb_id()),
    )
        .prop_map(
            |(
                identity,
                stats,
                content,
                context,
                keywords,
                embedding,
                embedder_model,
                derived_from_episode,
            )| Note {
                identity,
                stats,
                content,
                context,
                keywords,
                embedding,
                embedder_model,
                derived_from_episode,
            },
        )
}

fn arb_core_block() -> impl Strategy<Value = CoreBlock> {
    (
        arb_identity(),
        arb_stats(),
        any::<String>(),
        arb_block_kind(),
        prop::option::of(any::<String>()),
        arb_opt_json(),
        prop::option::of(arb_embedding()),
        prop::option::of(arb_embedder_model()),
    )
        .prop_map(
            |(
                identity,
                stats,
                content,
                block_kind,
                sensitivity,
                drift_baseline,
                embedding,
                embedder_model,
            )| CoreBlock {
                identity,
                stats,
                content,
                block_kind,
                sensitivity,
                drift_baseline,
                embedding,
                embedder_model,
            },
        )
}

fn arb_agent() -> impl Strategy<Value = Agent> {
    (
        arb_identity(),
        any::<String>(),
        any::<String>(),
        prop::option::of(any::<String>()),
        arb_trust_scores(),
        arb_agent_status(),
    )
        .prop_map(
            |(identity, public_key, model_family, model_version, trust_scores, status)| Agent {
                identity,
                public_key,
                model_family,
                model_version,
                trust_scores,
                status,
            },
        )
}

fn arb_session() -> impl Strategy<Value = Session> {
    (
        arb_identity(),
        arb_timestamp(),
        arb_opt_ts(),
        arb_id(),
        arb_json(),
    )
        .prop_map(
            |(identity, started_at, ended_at, owner_agent_id, metadata)| Session {
                identity,
                started_at,
                ended_at,
                owner_agent_id,
                metadata,
            },
        )
}

fn arb_provenance_record() -> impl Strategy<Value = ProvenanceRecord> {
    (
        arb_identity(),
        arb_id(),
        arb_id(),
        any::<String>(),
        prop::collection::vec(arb_id(), 0..3),
        any::<String>(),
        prop::option::of(any::<String>()),
        arb_f64(),
    )
        .prop_map(
            |(
                identity,
                subject_id,
                writer_agent_id,
                signature,
                source_episode_ids,
                model_family,
                model_version,
                trust_at_write,
            )| ProvenanceRecord {
                identity,
                subject_id,
                writer_agent_id,
                signature,
                source_episode_ids,
                model_family,
                model_version,
                trust_at_write,
            },
        )
}

fn arb_audit_event() -> impl Strategy<Value = AuditEvent> {
    (
        arb_identity(),
        arb_audit_kind(),
        arb_id(),
        arb_id(),
        arb_json(),
        any::<String>(),
        arb_timestamp(),
    )
        .prop_map(
            |(identity, kind, subject_id, actor_id, payload, signature, occurred_at)| AuditEvent {
                identity,
                kind,
                subject_id,
                actor_id,
                payload,
                signature,
                occurred_at,
            },
        )
}

fn arb_promotion() -> impl Strategy<Value = Promotion> {
    (
        arb_identity(),
        arb_id(),
        arb_f64(),
        any::<u64>(),
        arb_promotion_status(),
        arb_opt_ts(),
        prop::option::of(arb_id()),
    )
        .prop_map(
            |(identity, candidate_fact_id, posterior, k, status, resolved_at, promoted_fact_id)| {
                Promotion {
                    identity,
                    candidate_fact_id,
                    posterior,
                    k,
                    status,
                    resolved_at,
                    promoted_fact_id,
                }
            },
        )
}

fn arb_consolidation_cursor() -> impl Strategy<Value = ConsolidationCursor> {
    (
        arb_identity(),
        any::<String>(),
        prop::option::of(arb_id()),
        arb_opt_ts(),
        arb_json(),
    )
        .prop_map(
            |(identity, last_position, last_episode_id, last_processed_at, rule_versions)| {
                ConsolidationCursor {
                    identity,
                    last_position,
                    last_episode_id,
                    last_processed_at,
                    rule_versions,
                }
            },
        )
}

fn arb_schema_version() -> impl Strategy<Value = SchemaVersion> {
    (arb_identity(), any::<i64>(), arb_timestamp()).prop_map(
        |(identity, current_version, applied_at)| SchemaVersion {
            identity,
            current_version,
            applied_at,
        },
    )
}

fn arb_scope() -> impl Strategy<Value = Scope> {
    (arb_identity(), any::<String>(), any::<String>()).prop_map(|(identity, name, scope_kind)| {
        Scope {
            identity,
            name,
            scope_kind,
        }
    })
}

fn arb_recency_window() -> impl Strategy<Value = RecencyWindow> {
    (arb_identity(), any::<String>(), arb_opt_ts(), arb_opt_ts()).prop_map(
        |(identity, label, starts_at, ends_at)| RecencyWindow {
            identity,
            label,
            starts_at,
            ends_at,
        },
    )
}

fn arb_validity_anchor() -> impl Strategy<Value = ValidityAnchor> {
    (
        arb_identity(),
        arb_timestamp(),
        prop::option::of(any::<String>()),
    )
        .prop_map(|(identity, instant, label)| ValidityAnchor {
            identity,
            instant,
            label,
        })
}

proptest! {
    // value types
    #[test] fn rt_namespace(v in arb_namespace()) { round_trip(v); }
    #[test] fn rt_timestamp(v in arb_timestamp()) { round_trip(v); }
    #[test] fn rt_embedding(v in arb_embedding()) { round_trip(v); }
    #[test] fn rt_embedder_model(v in arb_embedder_model()) { round_trip(v); }
    #[test] fn rt_object_value(v in arb_object_value()) { round_trip(v); }
    #[test] fn rt_identity(v in arb_identity()) { round_trip(v); }
    #[test] fn rt_stats(v in arb_stats()) { round_trip(v); }
    #[test] fn rt_bitemporal(v in arb_bitemporal()) { round_trip(v); }
    #[test] fn rt_origin(v in arb_origin()) { round_trip(v); }
    #[test] fn rt_extraction(v in arb_extraction()) { round_trip(v); }
    #[test] fn rt_trust_scores(v in arb_trust_scores()) { round_trip(v); }
    #[test] fn rt_edge_label(v in arb_edge_label()) { round_trip(v); }

    // node kinds
    #[test] fn rt_episode(v in arb_episode()) { round_trip(v); }
    #[test] fn rt_fact(v in arb_fact()) { round_trip(v); }
    #[test] fn rt_entity(v in arb_entity()) { round_trip(v); }
    #[test] fn rt_skill(v in arb_skill()) { round_trip(v); }
    #[test] fn rt_bad_pattern(v in arb_bad_pattern()) { round_trip(v); }
    #[test] fn rt_note(v in arb_note()) { round_trip(v); }
    #[test] fn rt_core_block(v in arb_core_block()) { round_trip(v); }
    #[test] fn rt_agent(v in arb_agent()) { round_trip(v); }
    #[test] fn rt_session(v in arb_session()) { round_trip(v); }
    #[test] fn rt_provenance_record(v in arb_provenance_record()) { round_trip(v); }
    #[test] fn rt_audit_event(v in arb_audit_event()) { round_trip(v); }
    #[test] fn rt_promotion(v in arb_promotion()) { round_trip(v); }
    #[test] fn rt_consolidation_cursor(v in arb_consolidation_cursor()) { round_trip(v); }
    #[test] fn rt_schema_version(v in arb_schema_version()) { round_trip(v); }
    #[test] fn rt_scope(v in arb_scope()) { round_trip(v); }
    #[test] fn rt_recency_window(v in arb_recency_window()) { round_trip(v); }
    #[test] fn rt_validity_anchor(v in arb_validity_anchor()) { round_trip(v); }

    // edge kinds carrying data
    #[test] fn rt_mentions(v in arb_bitemporal().prop_map(|temporal| Mentions { temporal })) { round_trip(v); }
    #[test] fn rt_about(v in arb_bitemporal().prop_map(|temporal| About { temporal })) { round_trip(v); }
    #[test] fn rt_supports(v in arb_f64().prop_map(|weight| Supports { weight })) { round_trip(v); }
    #[test] fn rt_superseded_by(v in (any::<String>(), arb_bitemporal()).prop_map(|(reason, temporal)| SupersededBy { reason, temporal })) { round_trip(v); }
    #[test] fn rt_contradicts(v in (any::<String>(), arb_bitemporal()).prop_map(|(detected_by, temporal)| Contradicts { detected_by, temporal })) { round_trip(v); }
    #[test] fn rt_valid_at(v in arb_bitemporal().prop_map(|temporal| ValidAt { temporal })) { round_trip(v); }
    #[test] fn rt_derived_from(v in arb_timestamp().prop_map(|derived_at| DerivedFrom { derived_at })) { round_trip(v); }
    #[test] fn rt_attested_by(v in (arb_timestamp(), any::<String>(), prop::option::of(any::<String>())).prop_map(|(attested_at, signature, category)| AttestedBy { attested_at, signature, category })) { round_trip(v); }
    #[test] fn rt_promoted_to(v in arb_bitemporal().prop_map(|temporal| PromotedTo { temporal })) { round_trip(v); }
    #[test] fn rt_demoted_from(v in arb_bitemporal().prop_map(|temporal| DemotedFrom { temporal })) { round_trip(v); }
    #[test] fn rt_has_failure(v in arb_timestamp().prop_map(|observed_at| HasFailure { observed_at })) { round_trip(v); }
    #[test] fn rt_relates_to(v in (any::<String>(), arb_bitemporal()).prop_map(|(relationship_label, temporal)| RelatesTo { relationship_label, temporal })) { round_trip(v); }

    // marker edge kinds
    #[test] fn rt_in_scope(_ in Just(())) { round_trip(InScope); }
    #[test] fn rt_in_session(_ in Just(())) { round_trip(InSession); }
    #[test] fn rt_recent_in(_ in Just(())) { round_trip(RecentIn); }
    #[test] fn rt_depends_on(_ in Just(())) { round_trip(DependsOn); }
    #[test] fn rt_has_provenance(_ in Just(())) { round_trip(HasProvenance); }
    #[test] fn rt_audit(_ in Just(())) { round_trip(Audit); }
}
