//! Integration tests for the procedural-memory layer (05; M3.T04 PR-B).
//!
//! These pin the layer-2 contract over the L0 skill surface: a save assigns a monotonic version
//! and an audit trail; re-saving an unchanged procedure is a no-op while a body or capability
//! change cuts a new, audited version that deprecates the prior; outcomes move reliability and
//! reorder retrieval; retrieval fuses the problem-embedding and description signals, weights them
//! by a Beta-posterior reliability, surfaces only active versions, and degrades to BM25 when the
//! embedder is down; and an absent problem embedding is computed at save, fail-closed.

mod common;

use aionforge_domain::contracts::ProceduralMemory;
use aionforge_domain::ids::Id;
use aionforge_procedural::ProceduralError;
use aionforge_store::{BoundQuery, QueryResult, Store, Value};

use common::{FakeEmbedder, NOW, service, skill, skill_no_embedding, store, ts};

/// Count the `AuditEvent -AUDIT-> Skill` edges anchored to the skill version with this domain id.
fn audit_edges_into(store: &Store, skill_id: &Id) -> usize {
    let query =
        BoundQuery::new("MATCH (a:AuditEvent)-[:AUDIT]->(s:Skill {id: $sid}) RETURN a.id AS id")
            .bind_str("sid", skill_id.as_str())
            .expect("bind skill id");
    match store.execute(&query).expect("audit query") {
        QueryResult::Rows(rows) => rows.row_count(),
        _ => 0,
    }
}

/// The payload of the `skill_version_diff` audit anchored to the skill version with this id.
fn version_diff_payload(store: &Store, skill_id: &Id) -> serde_json::Value {
    let query = BoundQuery::new(
        "MATCH (a:AuditEvent)-[:AUDIT]->(s:Skill {id: $sid}) \
         WHERE a.kind = $kind RETURN a.payload AS payload",
    )
    .bind_str("sid", skill_id.as_str())
    .expect("bind skill id")
    .bind_str("kind", "skill_version_diff")
    .expect("bind kind");
    match store.execute(&query).expect("payload query") {
        QueryResult::Rows(rows) => {
            assert_eq!(rows.row_count(), 1, "exactly one version-diff audit");
            let idx = rows.column_index("payload").expect("payload column");
            match rows.value(0, idx).expect("payload cell") {
                Value::Json(json) => json.as_serde().clone(),
                other => panic!("payload is not JSON: {other:?}"),
            }
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[tokio::test]
async fn save_assigns_version_one_and_retrieves() {
    let store = store();
    let embedder = FakeEmbedder::new().with("find the alpha", [1.0, 0.0, 0.0, 0.0]);
    let svc = service(store.clone(), embedder);

    let id = svc
        .save_skill(skill(
            "alpha",
            "alpha body",
            "alpha solver",
            &["fs.read"],
            [1.0, 0.0, 0.0, 0.0],
        ))
        .await
        .expect("save");

    let (_, saved) = store.active_skill("alpha").expect("read").expect("active");
    assert_eq!(saved.version, 1);
    assert_eq!(saved.identity.id, id);

    let hits = svc
        .retrieve_skills("find the alpha".to_string(), 5)
        .await
        .expect("retrieve");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].skill.name, "alpha");
    assert!(hits[0].similarity > 0.0);
    // A fresh 0/0 skill scores the neutral Beta(1,1) prior.
    assert!((hits[0].reliability - 0.5).abs() < 1e-9);
    assert!((hits[0].score - hits[0].similarity * hits[0].reliability).abs() < 1e-12);
}

#[tokio::test]
async fn resaving_an_unchanged_procedure_is_a_noop() {
    let store = store();
    let svc = service(store.clone(), FakeEmbedder::new());

    let first = svc
        .save_skill(skill(
            "beta",
            "beta body",
            "beta",
            &["fs.read"],
            [1.0, 0.0, 0.0, 0.0],
        ))
        .await
        .expect("first save");
    let second = svc
        .save_skill(skill(
            "beta",
            "beta body",
            "beta",
            &["fs.read"],
            [1.0, 0.0, 0.0, 0.0],
        ))
        .await
        .expect("second save");

    assert_eq!(first, second, "an identical re-save returns the same id");
    assert_eq!(store.skill_versions("beta").expect("versions").len(), 1);
}

#[tokio::test]
async fn changing_the_body_cuts_a_new_version_and_deprecates_the_prior() {
    let store = store();
    let svc = service(store.clone(), FakeEmbedder::new());

    svc.save_skill(skill(
        "gamma",
        "v1 body",
        "gamma",
        &["fs.read"],
        [1.0, 0.0, 0.0, 0.0],
    ))
    .await
    .expect("v1");
    let v2 = svc
        .save_skill(skill(
            "gamma",
            "v2 body",
            "gamma",
            &["fs.read"],
            [1.0, 0.0, 0.0, 0.0],
        ))
        .await
        .expect("v2");

    let versions = store.skill_versions("gamma").expect("versions");
    assert_eq!(versions.len(), 2);
    assert!(versions[0].deprecated_at.is_some(), "v1 is deprecated");
    assert!(versions[1].deprecated_at.is_none(), "v2 is active");
    let (_, active) = store.active_skill("gamma").expect("read").expect("active");
    assert_eq!(active.version, 2);
    assert_eq!(active.identity.id, v2);

    // Save + Deprecate + VersionDiff, all anchored to the new version (L0 contract).
    assert_eq!(audit_edges_into(&store, &v2), 3);
}

#[tokio::test]
async fn a_capability_change_alone_cuts_an_audited_version() {
    let store = store();
    let svc = service(store.clone(), FakeEmbedder::new());

    // Same body, different declared capabilities: still a new, audited version.
    svc.save_skill(skill(
        "delta",
        "same body",
        "delta",
        &["a", "b"],
        [1.0, 0.0, 0.0, 0.0],
    ))
    .await
    .expect("v1");
    let v2 = svc
        .save_skill(skill(
            "delta",
            "same body",
            "delta",
            &["b", "c"],
            [1.0, 0.0, 0.0, 0.0],
        ))
        .await
        .expect("v2");

    assert_eq!(store.skill_versions("delta").expect("versions").len(), 2);

    let payload = version_diff_payload(&store, &v2);
    assert_eq!(payload["from_version"], serde_json::json!(1));
    assert_eq!(payload["to_version"], serde_json::json!(2));
    assert_eq!(payload["capabilities_added"], serde_json::json!(["c"]));
    assert_eq!(payload["capabilities_removed"], serde_json::json!(["a"]));
    assert_eq!(payload["body_changed"], serde_json::json!(false));
}

#[tokio::test]
async fn outcomes_reweight_retrieval_order() {
    let store = store();
    // Two equally-similar skills; only their proven reliability differs.
    let embedder = FakeEmbedder::new().with("do the task", [1.0, 0.0, 0.0, 0.0]);
    let svc = service(store.clone(), embedder);

    let winner = svc
        .save_skill(skill(
            "winner",
            "w",
            "do the task",
            &[],
            [1.0, 0.0, 0.0, 0.0],
        ))
        .await
        .expect("winner");
    let loser = svc
        .save_skill(skill(
            "loser",
            "l",
            "do the task",
            &[],
            [1.0, 0.0, 0.0, 0.0],
        ))
        .await
        .expect("loser");

    for _ in 0..5 {
        svc.record_outcome(winner.clone(), true).await.expect("win");
        svc.record_outcome(loser.clone(), false)
            .await
            .expect("loss");
    }

    let hits = svc
        .retrieve_skills("do the task".to_string(), 5)
        .await
        .expect("retrieve");
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].skill.name, "winner", "the proven skill ranks first");
    assert_eq!(hits[1].skill.name, "loser");
    assert!(hits[0].reliability > hits[1].reliability);
    // 5/0 -> 6/7 ; 0/5 -> 1/7 under Beta(1,1).
    assert!((hits[0].reliability - 6.0 / 7.0).abs() < 1e-9);
    assert!((hits[1].reliability - 1.0 / 7.0).abs() < 1e-9);
}

#[tokio::test]
async fn recording_an_outcome_against_an_unknown_id_errors() {
    let store = store();
    let svc = service(store, FakeEmbedder::new());
    let result = svc.record_outcome(Id::generate(), true).await;
    assert!(matches!(result, Err(ProceduralError::NotFound(_))));
}

#[tokio::test]
async fn an_absent_problem_embedding_is_computed_at_save() {
    let store = store();
    let embedder = FakeEmbedder::new().with("epsilon problem", [0.0, 0.0, 1.0, 0.0]);
    let svc = service(store.clone(), embedder);

    let id = svc
        .save_skill(skill_no_embedding(
            "epsilon",
            "body",
            "epsilon problem",
            &[],
        ))
        .await
        .expect("save");

    let saved = store.skill_by_id(&id).expect("read").expect("present");
    assert!(saved.problem_embedding.is_some(), "embedding was computed");
    assert_eq!(
        saved.embedder_model.expect("model").family,
        "fake",
        "the computing embedder's identity is recorded"
    );

    let hits = svc
        .retrieve_skills("epsilon problem".to_string(), 5)
        .await
        .expect("retrieve");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].skill.name, "epsilon");
}

#[tokio::test]
async fn a_down_embedder_fails_a_save_with_no_supplied_embedding() {
    let store = store();
    let svc = service(store, FakeEmbedder::down());
    let result = svc
        .save_skill(skill_no_embedding("zeta", "body", "zeta", &[]))
        .await;
    assert!(matches!(result, Err(ProceduralError::Embed(_))));
}

#[tokio::test]
async fn retrieval_degrades_to_bm25_when_the_embedder_is_down() {
    let store = store();
    // The skill carries its own embedding, so the save succeeds even with a down embedder; the
    // query side then falls back to the description's BM25 index.
    let svc = service(store, FakeEmbedder::down());
    svc.save_skill(skill(
        "eta",
        "body",
        "zeta keyword unique",
        &[],
        [1.0, 0.0, 0.0, 0.0],
    ))
    .await
    .expect("save");

    let hits = svc
        .retrieve_skills("zeta keyword unique".to_string(), 5)
        .await
        .expect("retrieve");
    assert_eq!(hits.len(), 1, "found via the lexical recall floor");
    assert_eq!(hits[0].skill.name, "eta");
}

#[tokio::test]
async fn deprecated_versions_are_not_retrieved() {
    let store = store();
    let embedder = FakeEmbedder::new().with("eta task", [1.0, 0.0, 0.0, 0.0]);
    let svc = service(store, embedder);

    svc.save_skill(skill(
        "eta",
        "v1 body",
        "eta task",
        &[],
        [1.0, 0.0, 0.0, 0.0],
    ))
    .await
    .expect("v1");
    svc.save_skill(skill(
        "eta",
        "v2 body",
        "eta task",
        &[],
        [1.0, 0.0, 0.0, 0.0],
    ))
    .await
    .expect("v2");

    let hits = svc
        .retrieve_skills("eta task".to_string(), 5)
        .await
        .expect("retrieve");
    assert_eq!(hits.len(), 1, "only the active version surfaces");
    assert_eq!(hits[0].skill.version, 2);
}

#[tokio::test]
async fn retrieval_honors_k_zero_and_truncation() {
    let store = store();
    let embedder = FakeEmbedder::new().with("shared problem", [1.0, 0.0, 0.0, 0.0]);
    let svc = service(store, embedder);

    for name in ["one", "two", "three"] {
        svc.save_skill(skill(
            name,
            name,
            "shared problem",
            &[],
            [1.0, 0.0, 0.0, 0.0],
        ))
        .await
        .expect("save");
    }

    assert!(
        svc.retrieve_skills("shared problem".to_string(), 0)
            .await
            .expect("k=0")
            .is_empty(),
        "k=0 yields nothing"
    );
    let hits = svc
        .retrieve_skills("shared problem".to_string(), 2)
        .await
        .expect("k=2");
    assert_eq!(hits.len(), 2, "the result is truncated to k");
}

#[tokio::test]
async fn changing_only_the_description_is_a_noop() {
    let store = store();
    let svc = service(store.clone(), FakeEmbedder::new());

    // The description is a recall surface, not part of the procedure, so editing it alone must
    // not cut a new version (is_unchanged excludes it).
    let first = svc
        .save_skill(skill(
            "theta",
            "same body",
            "original description",
            &["fs.read"],
            [1.0, 0.0, 0.0, 0.0],
        ))
        .await
        .expect("first save");
    let second = svc
        .save_skill(skill(
            "theta",
            "same body",
            "a totally different description",
            &["fs.read"],
            [1.0, 0.0, 0.0, 0.0],
        ))
        .await
        .expect("second save");

    assert_eq!(first, second, "a description-only edit returns the same id");
    assert_eq!(store.skill_versions("theta").expect("versions").len(), 1);
}

#[tokio::test]
async fn changing_body_and_capabilities_together_is_one_audited_version() {
    let store = store();
    let svc = service(store.clone(), FakeEmbedder::new());

    // Both change vectors at once: the body hash and the capabilities. The result is a single new
    // version whose diff records both.
    svc.save_skill(skill(
        "iota",
        "v1 body",
        "iota",
        &["a"],
        [1.0, 0.0, 0.0, 0.0],
    ))
    .await
    .expect("v1");
    let v2 = svc
        .save_skill(skill(
            "iota",
            "v2 body",
            "iota",
            &["b"],
            [1.0, 0.0, 0.0, 0.0],
        ))
        .await
        .expect("v2");

    assert_eq!(store.skill_versions("iota").expect("versions").len(), 2);
    let payload = version_diff_payload(&store, &v2);
    assert_eq!(payload["body_changed"], serde_json::json!(true));
    assert_eq!(payload["capabilities_added"], serde_json::json!(["b"]));
    assert_eq!(payload["capabilities_removed"], serde_json::json!(["a"]));
}

#[tokio::test]
async fn a_noop_resave_after_outcomes_preserves_the_counters() {
    let store = store();
    let svc = service(store.clone(), FakeEmbedder::new());

    let id = svc
        .save_skill(skill("kappa", "body", "kappa", &[], [1.0, 0.0, 0.0, 0.0]))
        .await
        .expect("first save");
    svc.record_outcome(id.clone(), true).await.expect("success");
    svc.record_outcome(id.clone(), true).await.expect("success");
    svc.record_outcome(id.clone(), false)
        .await
        .expect("failure");

    // An unchanged re-save is a no-op, so the earned reliability must survive — the reset only
    // happens when a genuinely new version is cut.
    let resave = svc
        .save_skill(skill("kappa", "body", "kappa", &[], [1.0, 0.0, 0.0, 0.0]))
        .await
        .expect("resave");
    assert_eq!(id, resave, "the no-op re-save returns the same id");

    let saved = store.skill_by_id(&id).expect("read").expect("present");
    assert_eq!(saved.success_count, 2, "counters are preserved, not reset");
    assert_eq!(saved.failure_count, 1);
    assert_eq!(store.skill_versions("kappa").expect("versions").len(), 1);
}

#[tokio::test]
async fn over_fetch_lets_a_proven_skill_outrank_an_unproven_near_match() {
    let store = store();
    // The query matches `unproven` slightly better by vector, and the descriptions share no terms
    // with it so the lexical signal stays out of the way. At k=1, the default over-fetch
    // (candidate_multiplier = 4) is what pulls the lower-similarity-but-proven skill into the
    // candidate pool so reliability can lift it to the top.
    let embedder = FakeEmbedder::new().with("zzz query phrase", [1.0, 0.0, 0.0, 0.0]);
    let svc = service(store, embedder);

    let proven = svc
        .save_skill(skill(
            "proven",
            "proven body",
            "alpha procedure",
            &[],
            [0.8, 0.2, 0.0, 0.0],
        ))
        .await
        .expect("proven");
    svc.save_skill(skill(
        "unproven",
        "unproven body",
        "beta procedure",
        &[],
        [1.0, 0.0, 0.0, 0.0],
    ))
    .await
    .expect("unproven");

    for _ in 0..10 {
        svc.record_outcome(proven.clone(), true).await.expect("win");
    }

    let hits = svc
        .retrieve_skills("zzz query phrase".to_string(), 1)
        .await
        .expect("retrieve");
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].skill.name, "proven",
        "the proven skill outranks the closer-but-unproven match",
    );
}

#[tokio::test]
async fn equal_score_ties_break_by_skill_id() {
    let store = store();
    // RRF scores by rank, so two distinct skills only tie exactly when they rank in *opposite*
    // orders across the two signals (each then collects the same multiset of ranks). Construct
    // that: `mu` wins the vector signal (exact embedding match) while `nu` wins the lexical signal
    // (its description carries both query terms). With equal weights and equal (0/0) reliability,
    // their fused scores are bit-for-bit equal, so only the id tie-break can decide the order.
    let embedder = FakeEmbedder::new().with("kappa lambda", [1.0, 0.0, 0.0, 0.0]);
    let svc = service(store, embedder);

    svc.save_skill(skill("mu", "mu", "kappa", &[], [1.0, 0.0, 0.0, 0.0]))
        .await
        .expect("mu");
    svc.save_skill(skill("nu", "nu", "kappa lambda", &[], [0.9, 0.1, 0.0, 0.0]))
        .await
        .expect("nu");

    let hits = svc
        .retrieve_skills("kappa lambda".to_string(), 5)
        .await
        .expect("retrieve");
    assert_eq!(hits.len(), 2);
    assert!(
        (hits[0].score - hits[1].score).abs() < 1e-12,
        "opposite-order ranks make the fused scores exactly equal: {} vs {}",
        hits[0].score,
        hits[1].score,
    );
    assert!(
        hits[0].skill.identity.id < hits[1].skill.identity.id,
        "an exact score tie breaks by ascending skill id, deterministically",
    );
}

#[tokio::test]
async fn expired_soft_forgotten_skills_are_not_retrieved() {
    let store = store();
    // A soft-forgotten skill is stamped `expired_at` by the M5 forgetting layer. Save one
    // directly through the L0 surface (the L2 save would reset `expired_at`), then confirm
    // retrieval excludes it even though the indexes still contain it.
    let mut forgotten = skill(
        "forgotten",
        "body",
        "forgotten task",
        &[],
        [1.0, 0.0, 0.0, 0.0],
    );
    forgotten.version = 1;
    forgotten.identity.expired_at = Some(ts(NOW));
    store.save_skill(&forgotten, None, &[]).expect("L0 save");

    let embedder = FakeEmbedder::new().with("forgotten task", [1.0, 0.0, 0.0, 0.0]);
    let svc = service(store, embedder);
    let hits = svc
        .retrieve_skills("forgotten task".to_string(), 5)
        .await
        .expect("retrieve");
    assert!(hits.is_empty(), "a soft-forgotten skill never surfaces");
}

#[tokio::test]
async fn retrieval_over_an_empty_store_yields_nothing() {
    // Both signals are empty when nothing is stored: the vector index returns no neighbors and the
    // text index no hits, so retrieval is a clean empty result, not an error.
    let store = store();
    let svc = service(store, FakeEmbedder::new());
    let hits = svc
        .retrieve_skills("anything at all".to_string(), 5)
        .await
        .expect("retrieve");
    assert!(hits.is_empty(), "an empty store retrieves nothing");
}
