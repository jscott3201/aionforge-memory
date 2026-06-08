//! Integration tests for the versioned skill store surface (02 §4.4, 05; M3.T04 PR-A).
//!
//! These pin the L0 contract the procedural-memory layer composes: a skill round-trips
//! through `save_skill` / `skill_by_*`; saving a new version deprecates the prior active one
//! in one atomic commit (deprecate-never-delete, at most one active per name); the version
//! diff is recorded as `AuditEvent -AUDIT-> Skill` provenance; outcome counters move without
//! touching the immutable procedure; and the generic `SearchKind::Skill` surface retrieves a
//! skill by problem embedding and by description.

mod common;

use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::procedural::Skill;
use aionforge_domain::time::Timestamp;
use aionforge_store::{SearchKind, Store};

use aionforge_domain::embedding::Embedding;

use common::{entity, identity, stats, store, ts};

const T1: &str = "2026-06-06T10:00:00-05:00[America/Chicago]";
const T2: &str = "2026-06-07T10:00:00-05:00[America/Chicago]";

/// A skill named `name` at `version`, ingested at `ingested`, with a 4-dim problem embedding.
fn skill(name: &str, version: i64, ingested: &str, embedding: [f32; 4]) -> Skill {
    let body = format!("{name} v{version} body");
    let mut id = identity(Id::generate());
    id.ingested_at = ts(ingested);
    Skill {
        identity: id,
        stats: stats(),
        name: name.to_string(),
        version,
        description: format!("solves the {name} problem"),
        problem_embedding: Some(Embedding::new(embedding.to_vec()).expect("valid embedding")),
        embedder_model: None,
        language: "python".to_string(),
        body: body.clone(),
        params: serde_json::json!({ "type": "object" }),
        preconditions: None,
        postconditions: None,
        capabilities: vec!["fs.read".to_string()],
        success_count: 0,
        failure_count: 0,
        mean_latency_ms: None,
        source_hash: ContentHash::of(body.as_bytes()),
        last_success_at: None,
        last_failure_at: None,
        deprecated_at: None,
        induced: false,
    }
}

/// A minimal audit event of `kind` about `subject` (the shape the layer-2 caller builds).
fn audit(kind: AuditKind, subject: &Id) -> AuditEvent {
    AuditEvent {
        identity: identity(Id::generate()),
        kind,
        subject_id: *subject,
        actor_id: Id::generate(),
        payload: serde_json::json!({}),
        signature: "test-signature".to_string(),
        occurred_at: ts(T1),
    }
}

/// Count the `AuditEvent -AUDIT-> Skill` edges into the skill with this domain id.
fn audit_edges_into(store: &Store, skill_id: &Id) -> usize {
    use aionforge_store::{BoundQuery, QueryResult};
    let query =
        BoundQuery::new("MATCH (a:AuditEvent)-[:AUDIT]->(s:Skill {id: $sid}) RETURN a.id AS id")
            .bind_uuid("sid", skill_id)
            .expect("bind skill id");
    match store.execute(&query).expect("audit query") {
        QueryResult::Rows(rows) => rows.row_count(),
        _ => 0,
    }
}

#[test]
fn a_saved_skill_round_trips_by_node_and_domain_id() {
    let store = store();
    let s = skill("deploy", 1, T1, [1.0, 0.0, 0.0, 0.0]);
    let save = audit(AuditKind::SkillSave, &s.identity.id);

    let node = store.save_skill(&s, None, &[save]).expect("save skill");

    let by_node = store
        .skill_by_node_id(node)
        .expect("read by node")
        .expect("skill present");
    let by_id = store
        .skill_by_id(&s.identity.id)
        .expect("read by id")
        .expect("skill present");
    assert_eq!(by_node, s, "skill round-trips byte-for-byte by node id");
    assert_eq!(by_id, s, "skill round-trips byte-for-byte by domain id");
    assert_eq!(
        audit_edges_into(&store, &s.identity.id),
        1,
        "the SkillSave audit is wired AUDIT-> the skill",
    );
}

#[test]
fn skill_node_by_id_resolves_the_outcome_bridge() {
    let store = store();
    let s = skill("deploy", 1, T1, [1.0, 0.0, 0.0, 0.0]);
    let node = store
        .save_skill(&s, None, &[audit(AuditKind::SkillSave, &s.identity.id)])
        .expect("save skill");

    // The domain-id -> node-id resolver the procedural layer uses to record an outcome against a
    // specific version returns the same node `save_skill` created.
    assert_eq!(
        store
            .skill_node_by_id(&s.identity.id)
            .expect("resolve by id"),
        Some(node),
        "the id resolves to the saved version's node",
    );
    assert!(
        store
            .skill_node_by_id(&Id::generate())
            .expect("resolve unknown")
            .is_none(),
        "an unknown id resolves to no node",
    );
}

#[test]
fn saving_a_new_version_deprecates_the_prior_active_one() {
    let store = store();
    let v1 = skill("deploy", 1, T1, [1.0, 0.0, 0.0, 0.0]);
    let v1_node = store
        .save_skill(&v1, None, &[audit(AuditKind::SkillSave, &v1.identity.id)])
        .expect("save v1");

    // The active version before v2 is v1.
    let (active_node, active) = store
        .active_skill("deploy")
        .expect("active lookup")
        .expect("an active version exists");
    assert_eq!(active_node, v1_node);
    assert_eq!(active.version, 1);

    // Save v2, deprecating v1 in the same commit, with the full version-diff audit set.
    let v2 = skill("deploy", 2, T2, [0.9, 0.1, 0.0, 0.0]);
    let diff_audits = [
        audit(AuditKind::SkillSave, &v2.identity.id),
        audit(AuditKind::SkillDeprecate, &v1.identity.id),
        audit(AuditKind::SkillVersionDiff, &v2.identity.id),
    ];
    store
        .save_skill(&v2, Some(v1_node), &diff_audits)
        .expect("save v2");

    // v1 is now deprecated (stamped at v2's ingested_at), v2 is the lone active version.
    let v1_after = store
        .skill_by_node_id(v1_node)
        .expect("read v1")
        .expect("v1 retained");
    assert_eq!(
        v1_after.deprecated_at.as_ref(),
        Some(&ts(T2)),
        "the prior version is deprecated at the new version's ingest instant, never deleted",
    );
    let (_, active_after) = store
        .active_skill("deploy")
        .expect("active lookup")
        .expect("v2 is active");
    assert_eq!(active_after.version, 2, "only the newest version is active");

    // The full history is retained in ascending version order.
    let versions = store.skill_versions("deploy").expect("versions");
    assert_eq!(
        versions.iter().map(|s| s.version).collect::<Vec<_>>(),
        vec![1, 2],
        "deprecate-never-delete keeps every version",
    );

    // The version diff is auditable: three events anchored to the new version's save.
    assert_eq!(
        audit_edges_into(&store, &v2.identity.id),
        3,
        "save + deprecate + version-diff audits are all recorded",
    );
}

#[test]
fn recording_outcomes_moves_only_the_reliability_stats() {
    let store = store();
    let s = skill("retry", 1, T1, [1.0, 0.0, 0.0, 0.0]);
    let node = store
        .save_skill(&s, None, &[audit(AuditKind::SkillSave, &s.identity.id)])
        .expect("save skill");

    let success_at: Timestamp = ts(T2);
    store
        .record_skill_outcome(node, true, &success_at)
        .expect("record success");
    store
        .record_skill_outcome(node, true, &success_at)
        .expect("record success");
    store
        .record_skill_outcome(node, false, &success_at)
        .expect("record failure");

    let after = store
        .skill_by_node_id(node)
        .expect("read skill")
        .expect("skill present");
    assert_eq!(after.success_count, 2, "two successes counted");
    assert_eq!(after.failure_count, 1, "one failure counted");
    assert_eq!(after.last_success_at.as_ref(), Some(&success_at));
    assert_eq!(after.last_failure_at.as_ref(), Some(&success_at));
    // The procedure itself is untouched — only the stats moved.
    assert_eq!(after.body, s.body);
    assert_eq!(after.capabilities, s.capabilities);
    assert_eq!(after.version, s.version);
}

#[test]
fn the_skill_search_surface_retrieves_by_embedding_and_description() {
    let store = store();
    let near = skill("near", 1, T1, [1.0, 0.0, 0.0, 0.0]);
    let far = skill("far", 1, T1, [0.0, 1.0, 0.0, 0.0]);
    let near_node = store
        .save_skill(
            &near,
            None,
            &[audit(AuditKind::SkillSave, &near.identity.id)],
        )
        .expect("save near");
    store
        .save_skill(&far, None, &[audit(AuditKind::SkillSave, &far.identity.id)])
        .expect("save far");

    // Problem-embedding vector search ranks the near skill first.
    let query = Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("query embedding");
    let hits = store
        .vector_search_ann(SearchKind::Skill, &query, 2)
        .expect("vector search");
    assert_eq!(
        hits.first().map(|h| h.node),
        Some(near_node),
        "the nearest problem embedding ranks first",
    );

    // BM25 over `description` finds the skill by its words.
    let text_hits = store
        .text_search(SearchKind::Skill, "near problem", 5)
        .expect("text search");
    assert!(
        text_hits.iter().any(|h| h.node == near_node),
        "the description text index retrieves the skill",
    );
}

#[test]
fn a_missing_skill_reads_back_as_none() {
    let store = store();
    assert!(
        store
            .skill_by_id(&Id::generate())
            .expect("lookup")
            .is_none(),
        "an unknown domain id yields no skill",
    );
    assert!(
        store.active_skill("nonexistent").expect("lookup").is_none(),
        "no active version for an unknown name",
    );
    assert!(
        store
            .skill_versions("nonexistent")
            .expect("lookup")
            .is_empty(),
        "no versions for an unknown name",
    );
}

#[test]
fn recording_an_outcome_against_a_non_skill_node_errors() {
    let store = store();
    // An entity node carries no skill counters: the outcome write must fail closed (a missing
    // `success_count`), not panic or silently no-op.
    let node = store
        .insert_entity(&entity("not-a-skill"))
        .expect("insert entity");
    assert!(
        store.record_skill_outcome(node, true, &ts(T1)).is_err(),
        "recording an outcome against a non-skill node is an error",
    );
}
