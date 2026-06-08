//! Integration tests for the bad-pattern store surface (02 §4.5, 05; M3.T05).
//!
//! These pin the L0 contract the procedural layer composes: a bad pattern round-trips, links to
//! the skill it was observed against via `HAS_FAILURE`, and bumps that skill's failure counter in
//! the same atomic commit; many patterns can link to one skill; and saving against a non-skill
//! node fails closed without creating anything.

mod common;

use aionforge_domain::embedding::Embedding;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::nodes::procedural::{BadPattern, Skill};

use common::{entity, identity, stats, store, ts};

const T1: &str = "2026-06-06T10:00:00-05:00[America/Chicago]";
const T2: &str = "2026-06-07T10:00:00-05:00[America/Chicago]";

fn skill(name: &str, embedding: [f32; 4]) -> Skill {
    let body = format!("{name} body");
    let mut id = identity(Id::generate());
    id.ingested_at = ts(T1);
    Skill {
        identity: id,
        stats: stats(),
        name: name.to_string(),
        version: 1,
        description: format!("solves the {name} problem"),
        problem_embedding: Some(Embedding::new(embedding.to_vec()).expect("valid embedding")),
        embedder_model: None,
        language: "python".to_string(),
        body: body.clone(),
        params: serde_json::json!({}),
        preconditions: None,
        postconditions: None,
        capabilities: Vec::new(),
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

fn bad_pattern(description: &str, embedding: [f32; 4], observed: &str) -> BadPattern {
    let mut id = identity(Id::generate());
    id.ingested_at = ts(observed);
    BadPattern {
        identity: id,
        stats: stats(),
        description: description.to_string(),
        embedding: Some(Embedding::new(embedding.to_vec()).expect("valid embedding")),
        embedder_model: None,
        observed_at: ts(observed),
    }
}

#[test]
fn a_bad_pattern_round_trips_links_and_bumps_the_failure_counter() {
    let store = store();
    let s = skill("deploy", [1.0, 0.0, 0.0, 0.0]);
    let skill_node = store.save_skill(&s, None, &[]).expect("save skill");

    let pattern = bad_pattern("rolled back on a bad config", [0.0, 1.0, 0.0, 0.0], T2);
    let pattern_node = store
        .save_bad_pattern(&pattern, skill_node)
        .expect("save bad pattern");

    let by_node = store
        .bad_pattern_by_node_id(pattern_node)
        .expect("read by node")
        .expect("pattern present");
    assert_eq!(
        by_node, pattern,
        "the bad pattern round-trips byte-for-byte"
    );

    let linked = store
        .bad_patterns_for_skill(skill_node)
        .expect("linked patterns");
    assert_eq!(linked.len(), 1, "the pattern links to its skill");
    assert_eq!(linked[0], pattern);

    // Recording a failure mode is a failure: the skill's counter moved in the same commit.
    let after = store
        .skill_by_node_id(skill_node)
        .expect("read skill")
        .expect("skill present");
    assert_eq!(after.failure_count, 1, "the failure counter was bumped");
    assert_eq!(after.last_failure_at.as_ref(), Some(&ts(T2)));
    assert_eq!(after.success_count, 0, "successes untouched");
}

#[test]
fn many_bad_patterns_link_to_one_skill_in_id_order() {
    let store = store();
    let skill_node = store
        .save_skill(&skill("retry", [1.0, 0.0, 0.0, 0.0]), None, &[])
        .expect("save skill");

    for i in 0..3 {
        let pattern = bad_pattern(&format!("failure {i}"), [0.0, 1.0, 0.0, 0.0], T2);
        store
            .save_bad_pattern(&pattern, skill_node)
            .expect("save pattern");
    }

    let linked = store
        .bad_patterns_for_skill(skill_node)
        .expect("linked patterns");
    assert_eq!(linked.len(), 3, "all three patterns link to the skill");
    let ids: Vec<&Id> = linked.iter().map(|p| &p.identity.id).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "patterns come back in ascending id order");

    let after = store
        .skill_by_node_id(skill_node)
        .expect("read")
        .expect("present");
    assert_eq!(after.failure_count, 3, "each pattern bumped the counter");
}

#[test]
fn a_skill_with_no_failures_has_no_patterns() {
    let store = store();
    let skill_node = store
        .save_skill(&skill("clean", [1.0, 0.0, 0.0, 0.0]), None, &[])
        .expect("save skill");
    assert!(
        store
            .bad_patterns_for_skill(skill_node)
            .expect("linked")
            .is_empty(),
        "no failures, no patterns",
    );
}

#[test]
fn saving_a_bad_pattern_against_a_non_skill_node_fails_closed() {
    let store = store();
    // An entity node carries no skill failure counter: the bump fails closed before anything is
    // created, so no orphan BadPattern is left behind.
    let entity_node = store
        .insert_entity(&entity("not-a-skill"))
        .expect("insert entity");
    let pattern = bad_pattern("should not persist", [0.0, 1.0, 0.0, 0.0], T2);
    assert!(
        store.save_bad_pattern(&pattern, entity_node).is_err(),
        "saving against a non-skill node is an error",
    );
}

#[test]
fn reading_a_wrong_kind_node_as_a_pattern_fails() {
    let store = store();
    let skill_node = store
        .save_skill(&skill("x", [1.0, 0.0, 0.0, 0.0]), None, &[])
        .expect("save skill");
    // A skill node is not a bad pattern; decoding it as one fails on the missing fields, the
    // expected closed behavior for a wrong-kind read.
    assert!(store.bad_pattern_by_node_id(skill_node).is_err());
}
