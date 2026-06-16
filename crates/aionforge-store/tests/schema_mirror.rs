//! The schema-mirror test (data-model §14): pins the full node/edge property surface.
//!
//! Hand-authored from the spec (§3 blocks, §4 node kinds, §5 edge kinds) and the domain
//! structs' `Option`-ness, deliberately independent of `catalog.rs`. Every node and edge
//! type's exact property set — each property's name, value kind, and `NOT NULL` /
//! `IMMUTABLE` / `UNIQUE` constraints — is asserted, so any catalog drift (a dropped or
//! renamed property, a changed type, a flipped constraint, an added field) fails this
//! test and is visible in review. Indexes and providers (§7–§9) are out of T04 scope and
//! not mirrored here.

use aionforge_store::{PropertyKind as K, PropertyShape, Store};

use jiff::Zoned;

#[derive(Debug)]
struct Prop {
    name: &'static str,
    kind: K,
    required: bool,
    immutable: bool,
    unique: bool,
}

fn prop(name: &'static str, kind: K, required: bool, immutable: bool, unique: bool) -> Prop {
    Prop {
        name,
        kind,
        required,
        immutable,
        unique,
    }
}
/// `NOT NULL`.
fn req(name: &'static str, kind: K) -> Prop {
    prop(name, kind, true, false, false)
}
/// Nullable.
fn opt(name: &'static str, kind: K) -> Prop {
    prop(name, kind, false, false, false)
}
/// `NOT NULL IMMUTABLE`.
fn req_imm(name: &'static str, kind: K) -> Prop {
    prop(name, kind, true, true, false)
}

/// Identity block carried by every kind (§3).
fn identity() -> Vec<Prop> {
    vec![
        prop("id", K::Uuid, true, true, true),
        req_imm("ingested_at", K::ZonedDateTime),
        req("namespace", K::String),
        opt("expired_at", K::ZonedDateTime),
    ]
}

/// Stats block carried by the 7 memory kinds (§3); mandatory in the domain.
fn stats() -> Vec<Prop> {
    vec![
        req("importance", K::Float),
        req("trust", K::Float),
        req("last_access", K::ZonedDateTime),
        req("access_count_recent", K::Uint),
        req("referenced_count", K::Uint),
        req("surprise", K::Float),
        req("is_pinned", K::Bool),
    ]
}

/// The four-timestamp bi-temporal block carried by the 8 bi-temporal edges (§5).
fn bitemporal() -> Vec<Prop> {
    vec![
        req("valid_from", K::ZonedDateTime),
        opt("valid_to", K::ZonedDateTime),
        req_imm("ingested_at", K::ZonedDateTime),
        opt("expired_at", K::ZonedDateTime),
    ]
}

/// Identity + stats + the per-kind fields.
fn memory(extra: Vec<Prop>) -> Vec<Prop> {
    let mut v = identity();
    v.extend(stats());
    v.extend(extra);
    v
}

/// Reduced identity (no stats) + the per-kind fields — forensic / control / anchor kinds.
fn reduced(extra: Vec<Prop>) -> Vec<Prop> {
    let mut v = identity();
    v.extend(extra);
    v
}

/// The bi-temporal block plus an edge's extra properties.
fn bt(extra: Vec<Prop>) -> Vec<Prop> {
    let mut v = bitemporal();
    v.extend(extra);
    v
}

fn expected_nodes() -> Vec<(&'static str, Vec<Prop>)> {
    vec![
        (
            "Episode",
            memory(vec![
                req("content", K::String),
                req("role", K::String),
                req_imm("captured_at", K::ZonedDateTime),
                req("agent_id", K::Uuid),
                opt("session_id", K::Uuid),
                req("content_hash", K::String),
                opt("embedding_v1", K::Vector),
                opt("embedder_model", K::String),
                req("consolidation_state", K::String),
                opt("origin", K::Json),
            ]),
        ),
        (
            "Fact",
            memory(vec![
                req("subject_id", K::Uuid),
                req("predicate", K::String),
                req("object_kind", K::String),
                opt("object_entity_id", K::Uuid),
                opt("object_value", K::Json),
                req("confidence", K::Float),
                req("status", K::String),
                opt("cooled_until", K::ZonedDateTime),
                req("statement", K::String),
                opt("embedding_v1", K::Vector),
                opt("embedder_model", K::String),
                opt("extraction", K::Json),
            ]),
        ),
        (
            "Entity",
            memory(vec![
                req("canonical_name", K::String),
                req("type", K::String),
                opt("aliases", K::List),
                opt("description", K::String),
                opt("embedding_v1", K::Vector),
                opt("embedder_model", K::String),
                opt("attributes", K::Json),
            ]),
        ),
        (
            "Skill",
            memory(vec![
                req("name", K::String),
                req("version", K::Int),
                req("description", K::String),
                opt("problem_embedding_v1", K::Vector),
                opt("embedder_model", K::String),
                req("language", K::String),
                req("body", K::String),
                req("params", K::Json),
                opt("preconditions", K::Json),
                opt("postconditions", K::Json),
                opt("capabilities", K::List),
                req("success_count", K::Uint),
                req("failure_count", K::Uint),
                opt("mean_latency_ms", K::Float),
                req("source_hash", K::String),
                opt("last_success_at", K::ZonedDateTime),
                opt("last_failure_at", K::ZonedDateTime),
                opt("deprecated_at", K::ZonedDateTime),
                req("induced", K::Bool),
            ]),
        ),
        (
            "BadPattern",
            memory(vec![
                req("description", K::String),
                opt("embedding_v1", K::Vector),
                opt("embedder_model", K::String),
                req_imm("observed_at", K::ZonedDateTime),
            ]),
        ),
        (
            "Note",
            memory(vec![
                req("content", K::String),
                opt("context", K::String),
                opt("keywords", K::List),
                opt("embedding_v1", K::Vector),
                opt("embedder_model", K::String),
                opt("derived_from_episode", K::Uuid),
            ]),
        ),
        (
            "CoreBlock",
            memory(vec![
                req("content", K::String),
                req("block_kind", K::String),
                opt("sensitivity", K::String),
                opt("drift_baseline", K::Json),
                opt("embedding_v1", K::Vector),
                opt("embedder_model", K::String),
            ]),
        ),
        (
            "Agent",
            reduced(vec![
                req_imm("public_key", K::String),
                req("model_family", K::String),
                opt("model_version", K::String),
                req("trust_scores", K::Json),
                req("status", K::String),
            ]),
        ),
        (
            "MemSession",
            reduced(vec![
                req("started_at", K::ZonedDateTime),
                opt("ended_at", K::ZonedDateTime),
                req("owner_agent_id", K::Uuid),
                req("metadata", K::Json),
            ]),
        ),
        (
            "ProvenanceRecord",
            reduced(vec![
                req("subject_id", K::Uuid),
                req("writer_agent_id", K::Uuid),
                req_imm("signature", K::String),
                opt("source_episode_ids", K::List),
                req("model_family", K::String),
                opt("model_version", K::String),
                req("trust_at_write", K::Float),
            ]),
        ),
        (
            "AuditEvent",
            reduced(vec![
                req("kind", K::String),
                req("subject_id", K::Uuid),
                req("actor_id", K::Uuid),
                req("payload", K::Json),
                // Mutable on purpose (02 §4.11 carve-out, M4.T06): the blank -> signed
                // upgrade latch lives in `audit::ensure_event`, which the DDL cannot
                // express. Every other signature in the schema (ProvenanceRecord,
                // ATTESTED_BY) stays IMMUTABLE.
                req("signature", K::String),
                req_imm("occurred_at", K::ZonedDateTime),
            ]),
        ),
        (
            "Promotion",
            reduced(vec![
                req("candidate_fact_id", K::Uuid),
                req("posterior", K::Float),
                req("k", K::Uint),
                req("status", K::String),
                opt("resolved_at", K::ZonedDateTime),
                opt("promoted_fact_id", K::Uuid),
            ]),
        ),
        (
            "ConsolidationCursor",
            reduced(vec![
                req("last_position", K::String),
                opt("last_episode_id", K::Uuid),
                opt("last_processed_at", K::ZonedDateTime),
                req("rule_versions", K::Json),
            ]),
        ),
        (
            "SchemaVersion",
            reduced(vec![
                req("current_version", K::Int),
                req("applied_at", K::ZonedDateTime),
            ]),
        ),
        (
            "Scope",
            reduced(vec![req("name", K::String), req("scope_kind", K::String)]),
        ),
        (
            "RecencyWindow",
            reduced(vec![
                req("label", K::String),
                opt("starts_at", K::ZonedDateTime),
                opt("ends_at", K::ZonedDateTime),
            ]),
        ),
        (
            "ValidityAnchor",
            reduced(vec![
                req("anchored_at", K::ZonedDateTime),
                opt("label", K::String),
            ]),
        ),
        // Work-tracking facet: Identity-only kinds (no Stats), so `reduced`. `work_status` is
        // a bounded STRING(32) but value_type stays K::String; the OPEN `level` is unbounded
        // STRING; `parent_id` is the nullable self-referential containment scalar.
        (
            "WorkItem",
            reduced(vec![
                req("title", K::String),
                opt("body", K::String),
                req("level", K::String),
                req("work_status", K::String),
                opt("parent_id", K::Uuid),
                req("ordinal", K::Uint),
            ]),
        ),
        (
            "Tag",
            reduced(vec![req("slug", K::String), opt("display", K::String)]),
        ),
    ]
}

fn expected_edges() -> Vec<(&'static str, Vec<Prop>)> {
    vec![
        ("MENTIONS", bitemporal()),
        ("ABOUT", bitemporal()),
        ("SUPPORTS", vec![req("weight", K::Float)]),
        ("SUPERSEDED_BY", bt(vec![req("reason", K::String)])),
        ("CONTRADICTS", bt(vec![req("detected_by", K::String)])),
        ("VALID_AT", bitemporal()),
        ("IN_SCOPE", vec![]),
        ("IN_SESSION", vec![]),
        ("RECENT_IN", vec![]),
        ("DEPENDS_ON", vec![]),
        (
            "DERIVED_FROM",
            vec![req_imm("derived_at", K::ZonedDateTime)],
        ),
        (
            "ATTESTED_BY",
            vec![
                req_imm("attested_at", K::ZonedDateTime),
                req_imm("signature", K::String),
                opt("category", K::String),
            ],
        ),
        ("PROMOTED_TO", bitemporal()),
        ("DEMOTED_FROM", bitemporal()),
        (
            "HAS_FAILURE",
            vec![req_imm("observed_at", K::ZonedDateTime)],
        ),
        ("RELATES_TO", bt(vec![req("relationship_label", K::String)])),
        ("HAS_PROVENANCE", vec![]),
        ("AUDIT", vec![]),
        ("HAS_TAG", vec![]),
    ]
}

fn assert_props(label: &str, actual: &[PropertyShape], expected: &[Prop]) {
    let actual_names: Vec<&str> = actual.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        actual.len(),
        expected.len(),
        "{label}: property count (declared: {actual_names:?})"
    );
    for want in expected {
        let got = actual
            .iter()
            .find(|p| p.name.as_str() == want.name)
            .unwrap_or_else(|| panic!("{label}: missing property {}", want.name));
        assert_eq!(
            got.value_type, want.kind,
            "{label}.{}: value kind",
            want.name
        );
        assert_eq!(
            got.required, want.required,
            "{label}.{}: NOT NULL",
            want.name
        );
        assert_eq!(
            got.immutable, want.immutable,
            "{label}.{}: IMMUTABLE",
            want.name
        );
        assert_eq!(got.unique, want.unique, "{label}.{}: UNIQUE", want.name);
    }
}

#[test]
fn schema_mirror_pins_the_full_node_and_edge_surface() {
    let now: Zoned = "2026-06-06T12:00:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime");
    let store = Store::open_in_memory_migrated(&now).expect("open and migrate");
    let snapshot = store
        .schema_snapshot()
        .expect("closed graph has a bound type");

    let nodes = expected_nodes();
    let edges = expected_edges();

    for (name, expected) in &nodes {
        let node = snapshot
            .node_type(name)
            .unwrap_or_else(|| panic!("node type {name} is declared"));
        assert_props(name, &node.properties, expected);
    }
    for (name, expected) in &edges {
        let edge = snapshot
            .edge_type(name)
            .unwrap_or_else(|| panic!("edge type {name} is declared"));
        assert_props(name, &edge.properties, expected);
    }

    // No kind exists beyond the mirror — catches an undocumented added type.
    assert_eq!(snapshot.node_types.len(), nodes.len(), "node kind count");
    assert_eq!(snapshot.edge_types.len(), edges.len(), "edge kind count");
}
