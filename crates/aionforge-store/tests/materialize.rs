//! Store-level tests for consolidation materialization of supersession and contradiction
//! (M2.T05a): `commit_consolidation_episode` applies the instructions in the flip txn,
//! non-destructively, and a replay of the same artifacts re-applies nothing.

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::edges::About;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::procedural::Skill;
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::value::ObjectValue;
use aionforge_store::{
    BoundQuery, ConsolidationArtifacts, ConsolidationCursor, Contradiction, FactKey,
    InducedSkillWrite, MaterializedFact, NodeId, QueryResult, Store, StoreConfig, Supersession,
    Value,
};

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

fn now() -> Timestamp {
    ts("2026-06-06T12:00:00Z[UTC]")
}

fn store() -> Store {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store
        .migrate(&ts("2026-01-01T00:00:00Z[UTC]"))
        .expect("migrate store");
    store
}

fn stats() -> Stats {
    Stats {
        importance: 0.5,
        trust: 0.8,
        last_access: ts("2026-06-06T10:00:00Z[UTC]"),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

fn namespace() -> Namespace {
    Namespace::Agent("eve".to_string())
}

fn identity(id: Id) -> Identity {
    Identity {
        id,
        ingested_at: ts("2026-06-06T09:00:00Z[UTC]"),
        namespace: namespace(),
        expired_at: None,
    }
}

/// Insert a subject entity, returning its domain id and node id.
fn insert_entity(store: &Store, name: &str) -> (Id, NodeId) {
    let id = Id::generate();
    let entity = Entity {
        identity: identity(id),
        stats: stats(),
        canonical_name: name.to_string(),
        entity_type: "Person".to_string(),
        aliases: Vec::new(),
        description: None,
        embedding: None,
        embedder_model: None,
        attributes: None,
    };
    let node = store.insert_entity(&entity).expect("insert entity");
    (id, node)
}

fn fact(subject_id: &Id, predicate: &str, object: ObjectValue, statement: &str) -> Fact {
    Fact {
        identity: identity(Id::generate()),
        stats: stats(),
        subject_id: *subject_id,
        predicate: predicate.to_string(),
        object,
        confidence: 0.9,
        status: FactStatus::Active,
        statement: statement.to_string(),
        embedding: None,
        embedder_model: None,
        extraction: None,
    }
}

fn open_window(from: &str) -> About {
    About {
        temporal: BiTemporal {
            valid_from: ts(from),
            valid_to: None,
            ingested_at: ts(from),
            expired_at: None,
        },
    }
}

/// Insert a `raw` episode (the flip target), returning its node id and value.
fn insert_episode(store: &Store) -> (NodeId, Episode) {
    let episode = Episode {
        identity: identity(Id::generate()),
        stats: stats(),
        content: "consolidation source".to_string(),
        role: Role::User,
        captured_at: ts("2026-06-06T09:00:00Z[UTC]"),
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(b"consolidation source"),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    let node = store.insert_episode(&episode).expect("insert episode");
    (node, episode)
}

fn cursor_at(episode: &Episode) -> ConsolidationCursor {
    ConsolidationCursor {
        last_position: ConsolidationCursor::watermark_for(episode),
        last_episode_id: Some(episode.identity.id),
        last_processed_at: Some(now()),
        rule_versions: serde_json::Value::Object(serde_json::Map::new()),
    }
}

/// Count `(a:Fact)-[:label]->(b:Fact)` edges between two fact ids.
fn edge_count(store: &Store, label: &str, a: &Id, b: &Id) -> u64 {
    // gql-ident-ok: `label` is a trusted static relationship name; the ids are bound.
    let query = BoundQuery::new(format!(
        "MATCH (a:Fact)-[r:{label}]->(b:Fact) WHERE a.id = $a AND b.id = $b RETURN count(r) AS n"
    ))
    .bind_uuid("a", a)
    .expect("bind a")
    .bind_uuid("b", b)
    .expect("bind b");
    match store.execute(&query).expect("count edges") {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Uint(n)) => *n,
            Some(Value::Int(n)) => u64::try_from(*n).unwrap_or(0),
            _ => 0,
        },
        _ => 0,
    }
}

/// The stored status string of the fact with this id ("active"/"superseded"/"quarantined").
fn fact_status(store: &Store, id: &Id) -> String {
    let query =
        BoundQuery::new("MATCH (f:Fact) WHERE f.id = $id RETURN f.status AS status LIMIT 1")
            .bind_uuid("id", id)
            .expect("bind id");
    match store.execute(&query).expect("status query") {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::String(s)) => s.as_str().to_string(),
            other => panic!("expected a status string, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

/// How many `Fact` nodes carry this id (1 once asserted; proves no duplicate on replay).
fn fact_count_by_id(store: &Store, id: &Id) -> usize {
    let query = BoundQuery::new("MATCH (f:Fact) WHERE f.id = $id RETURN f.id AS id")
        .bind_uuid("id", id)
        .expect("bind id");
    match store.execute(&query).expect("fact count") {
        QueryResult::Rows(rows) => rows.row_count(),
        _ => 0,
    }
}

/// How many `Entity` nodes carry this id (proves a re-minted id deduped rather than dup'd).
fn entity_count_by_id(store: &Store, id: &Id) -> usize {
    let query = BoundQuery::new("MATCH (e:Entity) WHERE e.id = $id RETURN e.id AS id")
        .bind_uuid("id", id)
        .expect("bind id");
    match store.execute(&query).expect("entity count") {
        QueryResult::Rows(rows) => rows.row_count(),
        _ => 0,
    }
}

fn reset_to_raw(store: &Store) {
    let query =
        BoundQuery::new("MATCH (e:Episode) SET e.consolidation_state = $raw RETURN e.id AS id")
            .bind_str("raw", "raw")
            .expect("bind raw");
    store.execute(&query).expect("reset episode to raw");
}

/// Total count of `(:Fact)-[:label]->(:Fact)` edges in the graph.
fn total_edges(store: &Store, label: &str) -> u64 {
    // gql-ident-ok: `label` is a trusted static relationship name.
    let query = BoundQuery::new(format!(
        "MATCH (:Fact)-[r:{label}]->(:Fact) RETURN count(r) AS n"
    ));
    match store.execute(&query).expect("count edges") {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Uint(n)) => *n,
            Some(Value::Int(n)) => u64::try_from(*n).unwrap_or(0),
            _ => 0,
        },
        _ => 0,
    }
}

#[test]
fn materialize_dedups_a_re_minted_entity_id_without_violating_the_unique_constraint() {
    // A resolution gate-miss can re-mint an existing entity's content-addressed id under a
    // case/whitespace-variant canonical name — the id derivation and the resolution gate share
    // one `normalize`, so the variant hashes to the same id. `Entity.id` is UNIQUE, so the
    // write must dedup by id, not by the exact `canonical_name`; otherwise the flip collides
    // and fails the episode. Here the committed entity is "New York"; the pass re-presents the
    // same id as a "new" entity named "new york".
    let store = store();
    let (id, _node) = insert_entity(&store, "New York");
    let (ep_node, episode) = insert_episode(&store);

    let variant = Entity {
        identity: identity(id),
        stats: stats(),
        canonical_name: "new york".to_string(),
        entity_type: "Person".to_string(),
        aliases: Vec::new(),
        description: None,
        embedding: None,
        embedder_model: None,
        attributes: None,
    };
    let about = fact(
        &id,
        "based_in",
        ObjectValue::Text("USA".to_string()),
        "New York is in the USA",
    );
    let mut artifacts = ConsolidationArtifacts::default();
    artifacts.new_entities.push(variant);
    artifacts.mentioned_entities.push(id);
    artifacts.facts.push(MaterializedFact {
        fact: about.clone(),
        about: open_window("2026-06-06T11:00:00Z[UTC]"),
    });

    store
        .commit_consolidation_episode(
            ep_node,
            ConsolidationState::Raw,
            ConsolidationState::Consolidated,
            &cursor_at(&episode),
            &now(),
            &artifacts,
        )
        .expect("commit must dedup by id, not collide on the UNIQUE id constraint");

    assert_eq!(
        entity_count_by_id(&store, &id),
        1,
        "the re-minted id collapses onto the existing node — no duplicate, no UNIQUE violation"
    );
    assert_eq!(
        fact_count_by_id(&store, &about.identity.id),
        1,
        "the fact about the deduped entity is written"
    );
}

#[test]
fn materialize_falls_back_to_canonical_name_across_id_schemes_but_respects_type() {
    // The fallback probe bridges a prior id scheme: an entity minted under a different id (a
    // pre-content-hash migration, say) is still found by exact canonical_name + type + namespace,
    // so a re-presented "new" entity dedups onto it instead of duplicating. The type/namespace
    // filters keep a same-name entity of a different type distinct (no false merge).
    let store = store();
    let (existing_id, _node) = insert_entity(&store, "Paris"); // type "Person"
    let (ep_node, episode) = insert_episode(&store);

    let typed = |name: &str, entity_type: &str| Entity {
        identity: identity(Id::generate()), // a fresh id, distinct from `existing_id`
        stats: stats(),
        canonical_name: name.to_string(),
        entity_type: entity_type.to_string(),
        aliases: Vec::new(),
        description: None,
        embedding: None,
        embedder_model: None,
        attributes: None,
    };
    // (a) Same name + type + namespace under a different id: id-probe misses, name fallback hits.
    let bridged = typed("Paris", "Person");
    // (b) Same name, different type: the type filter must keep it distinct.
    let other_type = typed("Paris", "City");

    let mut artifacts = ConsolidationArtifacts::default();
    artifacts.new_entities.push(bridged.clone());
    artifacts.new_entities.push(other_type.clone());
    artifacts.mentioned_entities.push(bridged.identity.id);
    artifacts.mentioned_entities.push(other_type.identity.id);

    store
        .commit_consolidation_episode(
            ep_node,
            ConsolidationState::Raw,
            ConsolidationState::Consolidated,
            &cursor_at(&episode),
            &now(),
            &artifacts,
        )
        .expect("commit");

    assert_eq!(
        entity_count_by_id(&store, &bridged.identity.id),
        0,
        "the bridged id deduped onto the existing node via the name fallback — it minted no node"
    );
    assert_eq!(
        entity_count_by_id(&store, &existing_id),
        1,
        "the existing Person:Paris remains the single canonical node"
    );
    assert_eq!(
        entity_count_by_id(&store, &other_type.identity.id),
        1,
        "a same-name entity of a different type stays distinct (type filter blocks a false merge)"
    );
}

#[test]
fn materialize_supersession_closes_window_and_is_idempotent() {
    let store = store();
    let (subject_id, subject_node) = insert_entity(&store, "Eve");

    // Incumbent current fact: (Eve, based_in, NYC), window open at 09:00.
    let old = fact(
        &subject_id,
        "based_in",
        ObjectValue::Text("NYC".to_string()),
        "Eve is based in NYC",
    );
    let old_node = store
        .assert_fact(
            &old,
            subject_node,
            &open_window("2026-06-06T09:00:00Z[UTC]"),
        )
        .expect("assert incumbent");

    // A later episode asserts (Eve, based_in, SF) and supersedes the incumbent.
    let (ep_node, episode) = insert_episode(&store);
    let new = fact(
        &subject_id,
        "based_in",
        ObjectValue::Text("SF".to_string()),
        "Eve is based in SF",
    );
    let mut artifacts = ConsolidationArtifacts::default();
    artifacts.facts.push(MaterializedFact {
        fact: new.clone(),
        about: open_window("2026-06-06T11:00:00Z[UTC]"),
    });
    artifacts.supersessions.push(Supersession {
        old_fact: FactKey {
            subject_id,
            predicate: "based_in".to_string(),
            object: ObjectValue::Text("NYC".to_string()),
        },
        new_fact: FactKey {
            subject_id,
            predicate: "based_in".to_string(),
            object: ObjectValue::Text("SF".to_string()),
        },
        reason: "newer assertion".to_string(),
        valid_from: ts("2026-06-06T11:00:00Z[UTC]"),
    });

    store
        .commit_consolidation_episode(
            ep_node,
            ConsolidationState::Raw,
            ConsolidationState::Consolidated,
            &cursor_at(&episode),
            &now(),
            &artifacts,
        )
        .expect("commit");

    // The new fact exists; the prior fact is preserved with a closed event-time window.
    assert_eq!(
        fact_count_by_id(&store, &new.identity.id),
        1,
        "new fact written"
    );
    assert_eq!(
        fact_count_by_id(&store, &old.identity.id),
        1,
        "prior fact preserved"
    );
    assert_eq!(
        edge_count(&store, "SUPERSEDED_BY", &old.identity.id, &new.identity.id),
        1,
        "one SUPERSEDED_BY edge"
    );
    let about = store
        .fact_about(old_node)
        .expect("fact_about")
        .expect("prior has ABOUT");
    assert_eq!(
        about.temporal.valid_to,
        Some(ts("2026-06-06T11:00:00Z[UTC]")),
        "prior event-time window closes at the supersession instant"
    );
    assert_eq!(fact_status(&store, &old.identity.id), "superseded");

    // Replay the same artifacts: the window is already closed and the edge already exists,
    // so nothing is re-applied.
    reset_to_raw(&store);
    store
        .commit_consolidation_episode(
            ep_node,
            ConsolidationState::Raw,
            ConsolidationState::Consolidated,
            &cursor_at(&episode),
            &now(),
            &artifacts,
        )
        .expect("replay commit");
    assert_eq!(
        edge_count(&store, "SUPERSEDED_BY", &old.identity.id, &new.identity.id),
        1,
        "replay adds no second SUPERSEDED_BY edge"
    );
    assert_eq!(
        store
            .fact_about(old_node)
            .expect("fact_about")
            .expect("prior has ABOUT")
            .temporal
            .valid_to,
        Some(ts("2026-06-06T11:00:00Z[UTC]")),
        "replay does not re-close the window"
    );
    assert_eq!(
        fact_count_by_id(&store, &new.identity.id),
        1,
        "replay adds no duplicate fact"
    );
}

#[test]
fn materialize_skips_an_unresolvable_supersession_without_failing_the_commit() {
    // A pass that emits an instruction referencing a fact that is neither in this txn nor
    // in the committed graph is a pass bug; materialization degrades gracefully — it drops
    // just that instruction (logged) so one bad key cannot wedge the whole flip — and the
    // rest of the consolidation still commits.
    let store = store();
    let (subject_id, _subject_node) = insert_entity(&store, "Eve");
    let (ep_node, episode) = insert_episode(&store);

    let new = fact(
        &subject_id,
        "based_in",
        ObjectValue::Text("SF".to_string()),
        "Eve is based in SF",
    );
    let mut artifacts = ConsolidationArtifacts::default();
    artifacts.facts.push(MaterializedFact {
        fact: new.clone(),
        about: open_window("2026-06-06T11:00:00Z[UTC]"),
    });
    // `old_fact` names a fact that was never asserted — it resolves to nothing.
    artifacts.supersessions.push(Supersession {
        old_fact: FactKey {
            subject_id: Id::generate(),
            predicate: "based_in".to_string(),
            object: ObjectValue::Text("Nowhere".to_string()),
        },
        new_fact: FactKey {
            subject_id,
            predicate: "based_in".to_string(),
            object: ObjectValue::Text("SF".to_string()),
        },
        reason: "orphan".to_string(),
        valid_from: ts("2026-06-06T11:00:00Z[UTC]"),
    });

    store
        .commit_consolidation_episode(
            ep_node,
            ConsolidationState::Raw,
            ConsolidationState::Consolidated,
            &cursor_at(&episode),
            &now(),
            &artifacts,
        )
        .expect("commit succeeds despite the orphan instruction");

    assert_eq!(
        fact_count_by_id(&store, &new.identity.id),
        1,
        "the well-formed fact is still written"
    );
    assert_eq!(
        total_edges(&store, "SUPERSEDED_BY"),
        0,
        "the unresolvable supersession wrote no edge"
    );
}

#[test]
fn materialize_contradiction_quarantines_the_source_and_is_idempotent() {
    let store = store();
    let (subject_id, subject_node) = insert_entity(&store, "Server");

    // Incumbent current fact: (Server, is_up, true).
    let incumbent = fact(
        &subject_id,
        "is_up",
        ObjectValue::Bool(true),
        "Server is up",
    );
    store
        .assert_fact(
            &incumbent,
            subject_node,
            &open_window("2026-06-06T09:00:00Z[UTC]"),
        )
        .expect("assert incumbent");

    // A new episode asserts the opposite and records a contradiction, quarantining the new.
    let (ep_node, episode) = insert_episode(&store);
    let conflicting = fact(
        &subject_id,
        "is_up",
        ObjectValue::Bool(false),
        "Server is down",
    );
    let mut artifacts = ConsolidationArtifacts::default();
    artifacts.facts.push(MaterializedFact {
        fact: conflicting.clone(),
        about: open_window("2026-06-06T11:00:00Z[UTC]"),
    });
    artifacts.contradictions.push(Contradiction {
        source_fact: FactKey {
            subject_id,
            predicate: "is_up".to_string(),
            object: ObjectValue::Bool(false),
        },
        target_fact: FactKey {
            subject_id,
            predicate: "is_up".to_string(),
            object: ObjectValue::Bool(true),
        },
        detected_by: "boolean-inversion".to_string(),
        quarantine_source: true,
        detected_at: ts("2026-06-06T11:00:00Z[UTC]"),
    });

    store
        .commit_consolidation_episode(
            ep_node,
            ConsolidationState::Raw,
            ConsolidationState::Consolidated,
            &cursor_at(&episode),
            &now(),
            &artifacts,
        )
        .expect("commit");

    // Both facts survive; the new fact is quarantined; the incumbent is untouched.
    assert_eq!(
        edge_count(
            &store,
            "CONTRADICTS",
            &conflicting.identity.id,
            &incumbent.identity.id
        ),
        1,
        "one CONTRADICTS edge from the new fact to the incumbent"
    );
    assert_eq!(fact_status(&store, &conflicting.identity.id), "quarantined");
    assert_eq!(
        fact_status(&store, &incumbent.identity.id),
        "active",
        "the high-trust incumbent is not silently overwritten"
    );

    // Replay: no duplicate edge, status unchanged.
    reset_to_raw(&store);
    store
        .commit_consolidation_episode(
            ep_node,
            ConsolidationState::Raw,
            ConsolidationState::Consolidated,
            &cursor_at(&episode),
            &now(),
            &artifacts,
        )
        .expect("replay commit");
    assert_eq!(
        edge_count(
            &store,
            "CONTRADICTS",
            &conflicting.identity.id,
            &incumbent.identity.id
        ),
        1,
        "replay adds no second CONTRADICTS edge"
    );
    assert_eq!(fact_status(&store, &conflicting.identity.id), "quarantined");
}

// --- Induced skills (M3.T06) ---------------------------------------------------------------

/// A minimal induced skill for the materializer: `induced = true`, content-addressed id, and a
/// `source_hash` derived from the body (so a different body is a different hash).
fn induced_skill(id_key: &str, name: &str, body: &str) -> Skill {
    Skill {
        identity: identity(Id::from_content_hash(id_key.as_bytes())),
        stats: stats(),
        name: name.to_string(),
        version: 1,
        description: "induced procedure".to_string(),
        problem_embedding: None,
        embedder_model: None,
        language: "text".to_string(),
        body: body.to_string(),
        params: serde_json::Value::Null,
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
        induced: true,
    }
}

#[test]
fn induced_skill_is_materialized_with_episode_lineage_and_replays_to_a_no_op() {
    let store = store();
    let (ep_node, episode) = insert_episode(&store);

    let skill = induced_skill(
        "induced-1",
        "induced/abc123",
        "do the thing\nthen the next thing",
    );
    let mut artifacts = ConsolidationArtifacts::default();
    artifacts.induced_skills = vec![InducedSkillWrite {
        skill: skill.clone(),
        deprecate_prior: None,
        audits: Vec::new(),
    }];

    store
        .commit_consolidation_episode(
            ep_node,
            ConsolidationState::Raw,
            ConsolidationState::Consolidated,
            &cursor_at(&episode),
            &now(),
            &artifacts,
        )
        .expect("commit induced skill");

    let stored = store
        .skill_by_id(&skill.identity.id)
        .expect("read skill")
        .expect("the induced skill is persisted");
    assert!(
        stored.induced,
        "the materialized skill keeps its induced flag"
    );
    assert_eq!(stored.body, skill.body, "the body round-trips verbatim");

    // Provenance edge: the induced skill derives from the source episode.
    let lineage = BoundQuery::new(
        "MATCH (s:Skill {id: $sid})-[:DERIVED_FROM]->(e:Episode) RETURN e.id AS id",
    )
    .bind_uuid("sid", skill.identity.id)
    .expect("bind sid");
    let QueryResult::Rows(rows) = store.execute(&lineage).expect("lineage query") else {
        panic!("expected rows");
    };
    assert_eq!(
        rows.row_count(),
        1,
        "exactly one DERIVED_FROM edge to the episode"
    );

    // Replay the same artifacts (the crash-recovery path): the id already exists → skip, no dup.
    reset_to_raw(&store);
    store
        .commit_consolidation_episode(
            ep_node,
            ConsolidationState::Raw,
            ConsolidationState::Consolidated,
            &cursor_at(&episode),
            &now(),
            &artifacts,
        )
        .expect("replay commit is a no-op");
    let again = store.execute(&BoundQuery::new("MATCH (s:Skill) RETURN s.id AS id"));
    let QueryResult::Rows(rows) = again.expect("count skills") else {
        panic!("expected rows");
    };
    assert_eq!(
        rows.row_count(),
        1,
        "replay materializes no second induced skill"
    );
}

#[test]
fn two_bodies_under_one_induced_name_fail_closed() {
    let store = store();
    let (ep_node, episode) = insert_episode(&store);

    // Same name, different bodies → different source_hash → an inducer invariant violation.
    let mut artifacts = ConsolidationArtifacts::default();
    artifacts.induced_skills = vec![
        InducedSkillWrite {
            skill: induced_skill("induced-a", "induced/dup", "body alpha alpha alpha"),
            deprecate_prior: None,
            audits: Vec::new(),
        },
        InducedSkillWrite {
            skill: induced_skill("induced-b", "induced/dup", "body beta beta beta"),
            deprecate_prior: None,
            audits: Vec::new(),
        },
    ];

    let result = store.commit_consolidation_episode(
        ep_node,
        ConsolidationState::Raw,
        ConsolidationState::Consolidated,
        &cursor_at(&episode),
        &now(),
        &artifacts,
    );
    assert!(
        result.is_err(),
        "a dup-name conflict fails the commit closed"
    );

    // The transaction rolled back: no induced skill was written, and the episode is still raw.
    let skills = store.execute(&BoundQuery::new("MATCH (s:Skill) RETURN s.id AS id"));
    let QueryResult::Rows(rows) = skills.expect("count skills") else {
        panic!("expected rows");
    };
    assert_eq!(
        rows.row_count(),
        0,
        "nothing is published when the guard fires"
    );
}
