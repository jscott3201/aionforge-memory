//! Integration tests for the bad-pattern layer of procedural memory (05; M3.T05).
//!
//! These pin the three acceptance criteria: recording a failure produces a linked bad pattern and
//! bumps the skill's failure counter; retrieval surfaces a skill's failure modes alongside it with
//! their relevance to the query; and a failure mode that matches the current problem weighs the
//! skill down — isolated from the reliability drop by giving the compared skills equal failure
//! counts. A query-time embedder outage still surfaces the patterns, just without penalty.

mod common;

use aionforge_domain::contracts::ProceduralMemory;
use aionforge_domain::ids::Id;
use aionforge_procedural::{ProceduralConfig, ProceduralError};

use common::{FakeEmbedder, service, service_with, skill, store};

#[tokio::test]
async fn record_failure_links_a_pattern_bumps_the_counter_and_surfaces_it() {
    let store = store();
    let embedder = FakeEmbedder::new()
        .with("gamma problem", [1.0, 0.0, 0.0, 0.0])
        .with("gamma timed out", [1.0, 0.0, 0.0, 0.0]);
    let svc = service(store.clone(), embedder);

    let id = svc
        .save_skill(skill(
            "gamma",
            "body",
            "gamma problem",
            &[],
            [1.0, 0.0, 0.0, 0.0],
        ))
        .await
        .expect("save");
    let pattern_id = svc
        .record_failure(id.clone(), "gamma timed out".to_string())
        .await
        .expect("record failure");
    assert_ne!(pattern_id, id, "the pattern has its own id");

    let saved = store.skill_by_id(&id).expect("read").expect("present");
    assert_eq!(saved.failure_count, 1, "the failure counter was bumped");

    let hits = svc
        .retrieve_skills("gamma problem".to_string(), 5)
        .await
        .expect("retrieve");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].bad_patterns.len(), 1, "the failure mode surfaces");
    assert_eq!(
        hits[0].bad_patterns[0].pattern.description,
        "gamma timed out"
    );
    assert!(
        hits[0].bad_patterns[0].query_similarity >= 0.7,
        "the pattern is relevant to the query",
    );
}

#[tokio::test]
async fn recording_a_failure_against_an_unknown_skill_errors() {
    let store = store();
    let svc = service(store, FakeEmbedder::new());
    let result = svc.record_failure(Id::generate(), "boom".to_string()).await;
    assert!(matches!(result, Err(ProceduralError::NotFound(_))));
}

#[tokio::test]
async fn record_failure_fails_closed_when_the_embedder_is_down() {
    let store = store();
    // The skill carries its own embedding, so the save succeeds even with a down embedder; the
    // failure description then cannot be embedded, so the record fails closed.
    let svc = service(store, FakeEmbedder::down());
    let id = svc
        .save_skill(skill("delta", "body", "delta", &[], [1.0, 0.0, 0.0, 0.0]))
        .await
        .expect("save");
    let result = svc
        .record_failure(id, "cannot embed this".to_string())
        .await;
    assert!(matches!(result, Err(ProceduralError::Embed(_))));
}

#[tokio::test]
async fn a_query_relevant_failure_mode_penalizes_only_that_skill() {
    let store = store();
    // Both skills match the query equally and each takes exactly one failure (equal reliability).
    // Only beta's failure mode is relevant to the query, so only beta is penalized.
    let embedder = FakeEmbedder::new()
        .with("do the shared task", [1.0, 0.0, 0.0, 0.0]) // query
        .with("network timeout on slow links", [0.0, 1.0, 0.0, 0.0]) // alpha failure: irrelevant
        .with("fails on the shared task input", [1.0, 0.0, 0.0, 0.0]); // beta failure: relevant
    let svc = service(store, embedder);

    let alpha = svc
        .save_skill(skill(
            "alpha",
            "a",
            "do the shared task",
            &[],
            [1.0, 0.0, 0.0, 0.0],
        ))
        .await
        .expect("alpha");
    let beta = svc
        .save_skill(skill(
            "beta",
            "b",
            "do the shared task",
            &[],
            [1.0, 0.0, 0.0, 0.0],
        ))
        .await
        .expect("beta");
    svc.record_failure(alpha, "network timeout on slow links".to_string())
        .await
        .expect("alpha failure");
    svc.record_failure(beta, "fails on the shared task input".to_string())
        .await
        .expect("beta failure");

    let hits = svc
        .retrieve_skills("do the shared task".to_string(), 5)
        .await
        .expect("retrieve");
    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits[0].skill.name, "alpha",
        "the skill whose failure mode is irrelevant to this query ranks first",
    );
    assert_eq!(hits[1].skill.name, "beta");

    let alpha_hit = hits.iter().find(|h| h.skill.name == "alpha").unwrap();
    let beta_hit = hits.iter().find(|h| h.skill.name == "beta").unwrap();
    // Equal reliability (one failure each), so the gap is the bad-pattern penalty, isolated:
    assert!((alpha_hit.reliability - beta_hit.reliability).abs() < 1e-12);
    // alpha's failure mode is irrelevant → no penalty → score is exactly similarity * reliability.
    assert!(
        (alpha_hit.score - alpha_hit.similarity * alpha_hit.reliability).abs() < 1e-12,
        "an irrelevant failure mode does not penalize",
    );
    // beta's failure mode is relevant → penalized below similarity * reliability.
    assert!(
        beta_hit.score < beta_hit.similarity * beta_hit.reliability - 1e-9,
        "a query-relevant failure mode weighs the skill down",
    );
    assert!(beta_hit.bad_patterns[0].query_similarity >= 0.7);
    assert!(alpha_hit.bad_patterns[0].query_similarity < 0.7);
}

#[tokio::test]
async fn a_down_embedder_at_retrieval_still_surfaces_failures_without_penalty() {
    let store = store();
    // Record the failure with a working embedder, then retrieve through a down one over the same
    // store: the pattern still surfaces (with similarity 0) and applies no penalty.
    let up = service(
        store.clone(),
        FakeEmbedder::new()
            .with("epsilon task", [1.0, 0.0, 0.0, 0.0])
            .with("epsilon broke", [1.0, 0.0, 0.0, 0.0]),
    );
    let id = up
        .save_skill(skill(
            "epsilon",
            "body",
            "epsilon task",
            &[],
            [1.0, 0.0, 0.0, 0.0],
        ))
        .await
        .expect("save");
    up.record_failure(id.clone(), "epsilon broke".to_string())
        .await
        .expect("record failure");

    let down = service(store, FakeEmbedder::down());
    let hits = down
        .retrieve_skills("epsilon task".to_string(), 5)
        .await
        .expect("retrieve via BM25");
    assert_eq!(hits.len(), 1, "found via the lexical recall floor");
    assert_eq!(
        hits[0].bad_patterns.len(),
        1,
        "the failure mode still surfaces"
    );
    assert_eq!(
        hits[0].bad_patterns[0].query_similarity, 0.0,
        "no query embedding means no relevance score",
    );
    assert!(
        (hits[0].score - hits[0].similarity * hits[0].reliability).abs() < 1e-12,
        "a degraded query applies no bad-pattern penalty",
    );
}

#[tokio::test]
async fn multiple_relevant_failure_modes_scale_the_penalty() {
    let store = store();
    // Both failure descriptions match the query, so the skill carries two query-relevant patterns:
    // the penalty is 1/(1 + 0.5*2) = 0.5, harder than the 0.667 a single one would apply.
    let embedder = FakeEmbedder::new()
        .with("multi task", [1.0, 0.0, 0.0, 0.0])
        .with("fail one", [1.0, 0.0, 0.0, 0.0])
        .with("fail two", [1.0, 0.0, 0.0, 0.0]);
    let svc = service(store, embedder);

    let id = svc
        .save_skill(skill(
            "multi",
            "body",
            "multi task",
            &[],
            [1.0, 0.0, 0.0, 0.0],
        ))
        .await
        .expect("save");
    svc.record_failure(id.clone(), "fail one".to_string())
        .await
        .expect("fail one");
    svc.record_failure(id, "fail two".to_string())
        .await
        .expect("fail two");

    let hits = svc
        .retrieve_skills("multi task".to_string(), 5)
        .await
        .expect("retrieve");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].bad_patterns.len(), 2, "both failure modes surface");
    assert!(
        hits[0]
            .bad_patterns
            .iter()
            .all(|b| b.query_similarity >= 0.7)
    );
    // The penalty used count = 2 (multiplier 0.5), not count = 1 (0.667).
    let expected = hits[0].similarity * hits[0].reliability * 0.5;
    assert!(
        (hits[0].score - expected).abs() < 1e-9,
        "two relevant patterns multiply the score by 0.5",
    );
}

#[tokio::test]
async fn the_relevance_threshold_is_inclusive() {
    let store = store();
    // An orthogonal pattern scores cosine 0.0 against the query. With the threshold set to 0.0,
    // `0.0 >= 0.0` must count — pinning that the relevance comparison includes its boundary.
    let embedder = FakeEmbedder::new()
        .with("edge task", [1.0, 0.0, 0.0, 0.0])
        .with("orthogonal failure", [0.0, 1.0, 0.0, 0.0]);
    let config = ProceduralConfig {
        bad_pattern_similarity_threshold: 0.0,
        ..ProceduralConfig::default()
    };
    let svc = service_with(store, embedder, config);

    let id = svc
        .save_skill(skill(
            "edge",
            "body",
            "edge task",
            &[],
            [1.0, 0.0, 0.0, 0.0],
        ))
        .await
        .expect("save");
    svc.record_failure(id, "orthogonal failure".to_string())
        .await
        .expect("record failure");

    let hits = svc
        .retrieve_skills("edge task".to_string(), 5)
        .await
        .expect("retrieve");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].bad_patterns[0].query_similarity, 0.0);
    // At threshold 0.0 the boundary pattern counts, so a penalty applies.
    assert!(
        hits[0].score < hits[0].similarity * hits[0].reliability - 1e-9,
        "a pattern exactly at the threshold counts toward the penalty",
    );
}

#[tokio::test]
async fn surfaced_failure_modes_are_ordered_by_relevance() {
    let store = store();
    let embedder = FakeEmbedder::new()
        .with("order task", [1.0, 0.0, 0.0, 0.0])
        .with("most relevant", [1.0, 0.0, 0.0, 0.0]) // cosine 1.0
        .with("somewhat relevant", [0.8, 0.6, 0.0, 0.0]) // cosine 0.8
        .with("not relevant", [0.0, 1.0, 0.0, 0.0]); // cosine 0.0
    let svc = service(store, embedder);

    let id = svc
        .save_skill(skill(
            "order",
            "body",
            "order task",
            &[],
            [1.0, 0.0, 0.0, 0.0],
        ))
        .await
        .expect("save");
    // Record out of order; retrieval must still return them most-relevant first.
    svc.record_failure(id.clone(), "somewhat relevant".to_string())
        .await
        .expect("p2");
    svc.record_failure(id.clone(), "not relevant".to_string())
        .await
        .expect("p3");
    svc.record_failure(id, "most relevant".to_string())
        .await
        .expect("p1");

    let hits = svc
        .retrieve_skills("order task".to_string(), 5)
        .await
        .expect("retrieve");
    let sims: Vec<f64> = hits[0]
        .bad_patterns
        .iter()
        .map(|b| b.query_similarity)
        .collect();
    assert_eq!(sims.len(), 3);
    assert!(
        sims.windows(2).all(|w| w[0] >= w[1]),
        "patterns are ordered by query relevance, descending: {sims:?}",
    );
}

#[tokio::test]
async fn a_zero_weight_disables_the_penalty() {
    let store = store();
    // Even a fully query-relevant failure mode must not move the score when the weight is zero.
    let embedder = FakeEmbedder::new()
        .with("zero task", [1.0, 0.0, 0.0, 0.0])
        .with("relevant failure", [1.0, 0.0, 0.0, 0.0]);
    let config = ProceduralConfig {
        bad_pattern_weight: 0.0,
        ..ProceduralConfig::default()
    };
    let svc = service_with(store, embedder, config);

    let id = svc
        .save_skill(skill(
            "zero",
            "body",
            "zero task",
            &[],
            [1.0, 0.0, 0.0, 0.0],
        ))
        .await
        .expect("save");
    svc.record_failure(id, "relevant failure".to_string())
        .await
        .expect("record failure");

    let hits = svc
        .retrieve_skills("zero task".to_string(), 5)
        .await
        .expect("retrieve");
    assert_eq!(hits[0].bad_patterns.len(), 1, "the pattern still surfaces");
    assert!(
        hits[0].bad_patterns[0].query_similarity >= 0.7,
        "it is relevant"
    );
    assert!(
        (hits[0].score - hits[0].similarity * hits[0].reliability).abs() < 1e-12,
        "a zero weight applies no penalty even for a relevant pattern",
    );
}

#[tokio::test]
async fn all_failure_modes_surface_even_the_irrelevant_ones() {
    let store = store();
    // One relevant, two irrelevant: all three surface (transparency), but only the relevant one
    // counts toward the penalty.
    let embedder = FakeEmbedder::new()
        .with("surface task", [1.0, 0.0, 0.0, 0.0])
        .with("relevant", [1.0, 0.0, 0.0, 0.0])
        .with("unrelated one", [0.0, 1.0, 0.0, 0.0])
        .with("unrelated two", [0.0, 0.0, 1.0, 0.0]);
    let svc = service(store, embedder);

    let id = svc
        .save_skill(skill(
            "surface",
            "body",
            "surface task",
            &[],
            [1.0, 0.0, 0.0, 0.0],
        ))
        .await
        .expect("save");
    for desc in ["relevant", "unrelated one", "unrelated two"] {
        svc.record_failure(id.clone(), desc.to_string())
            .await
            .expect("record failure");
    }

    let hits = svc
        .retrieve_skills("surface task".to_string(), 5)
        .await
        .expect("retrieve");
    assert_eq!(hits[0].bad_patterns.len(), 3, "all failure modes surface");
    let relevant = hits[0]
        .bad_patterns
        .iter()
        .filter(|b| b.query_similarity >= 0.7)
        .count();
    assert_eq!(relevant, 1, "only one is query-relevant");
    // Penalty reflects count = 1 (0.667), not 3.
    let expected = hits[0].similarity * hits[0].reliability * (1.0 / 1.5);
    assert!((hits[0].score - expected).abs() < 1e-9);
}
