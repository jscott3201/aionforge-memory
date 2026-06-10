//! Store-level tests for consolidation materialization of summary notes (M2.T06):
//! `commit_consolidation_episode` writes a content-addressed `Note` and wires its
//! `Note -DERIVED_FROM-> Fact` lineage in the flip txn, idempotently and non-lossily.

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::edges::About;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::value::ObjectValue;
use aionforge_store::{
    BoundQuery, ConsolidationArtifacts, ConsolidationCursor, FactKey, MaterializedFact,
    MaterializedNote, NodeId, QueryResult, Store, StoreConfig, Value,
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
    Namespace::Agent("alice".to_string())
}

fn identity(id: Id) -> Identity {
    Identity {
        id,
        ingested_at: ts("2026-06-06T09:00:00Z[UTC]"),
        namespace: namespace(),
        expired_at: None,
    }
}

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
        cooled_until: None,
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

/// Commit one consolidation episode (raw -> consolidated) with the given artifacts.
fn commit(store: &Store, ep_node: NodeId, episode: &Episode, artifacts: &ConsolidationArtifacts) {
    store
        .commit_consolidation_episode(
            ep_node,
            ConsolidationState::Raw,
            ConsolidationState::Consolidated,
            &cursor_at(episode),
            &now(),
            artifacts,
        )
        .expect("commit consolidation episode");
}

fn reset_to_raw(store: &Store) {
    let query =
        BoundQuery::new("MATCH (e:Episode) SET e.consolidation_state = $raw RETURN e.id AS id")
            .bind_str("raw", "raw")
            .expect("bind raw");
    store.execute(&query).expect("reset episode to raw");
}

/// A summary note anchored on a subject, derived from one episode, with an explicit id —
/// the same content-addressed id the summarizer derives from a source set, so a replay
/// produces the same id and dedups.
fn note_with_id(id: Id, content: &str, episode_id: &Id) -> Note {
    Note {
        identity: identity(id),
        stats: stats(),
        content: content.to_string(),
        context: None,
        keywords: vec!["alice".to_string(), "aionforge".to_string()],
        embedding: None,
        embedder_model: None,
        derived_from_episode: Some(*episode_id),
    }
}

/// How many `Note` nodes carry this id (1 once written; 1 still after replay).
fn note_count_by_id(store: &Store, id: &Id) -> usize {
    let query = BoundQuery::new("MATCH (n:Note) WHERE n.id = $id RETURN n.id AS id")
        .bind_uuid("id", id)
        .expect("bind id");
    match store.execute(&query).expect("note count") {
        QueryResult::Rows(rows) => rows.row_count(),
        _ => 0,
    }
}

/// How many `Fact` nodes carry this id (proves the source facts are untouched).
fn fact_count_by_id(store: &Store, id: &Id) -> usize {
    let query = BoundQuery::new("MATCH (f:Fact) WHERE f.id = $id RETURN f.id AS id")
        .bind_uuid("id", id)
        .expect("bind id");
    match store.execute(&query).expect("fact count") {
        QueryResult::Rows(rows) => rows.row_count(),
        _ => 0,
    }
}

/// Total `(:Note)-[:DERIVED_FROM]->(:Fact)` lineage edges in the graph.
fn note_lineage_edges(store: &Store) -> u64 {
    let query = BoundQuery::new("MATCH (:Note)-[r:DERIVED_FROM]->(:Fact) RETURN count(r) AS n");
    match store.execute(&query).expect("count lineage") {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Uint(n)) => *n,
            Some(Value::Int(n)) => u64::try_from(*n).unwrap_or(0),
            _ => 0,
        },
        _ => 0,
    }
}

fn fact_key(subject_id: &Id, predicate: &str, object: ObjectValue) -> FactKey {
    FactKey {
        subject_id: *subject_id,
        predicate: predicate.to_string(),
        object,
    }
}

#[test]
fn materialize_note_writes_lineage_to_committed_facts_and_is_idempotent() {
    let store = store();
    let (subject_id, subject_node) = insert_entity(&store, "Alice");

    // Two committed facts the summary will roll up.
    let f1 = fact(
        &subject_id,
        "works_on",
        ObjectValue::Text("Aionforge".to_string()),
        "Alice works on Aionforge",
    );
    store
        .assert_fact(&f1, subject_node, &open_window("2026-06-06T09:00:00Z[UTC]"))
        .expect("assert f1");
    let f2 = fact(
        &subject_id,
        "based_in",
        ObjectValue::Text("NYC".to_string()),
        "Alice is based in NYC",
    );
    store
        .assert_fact(&f2, subject_node, &open_window("2026-06-06T09:00:00Z[UTC]"))
        .expect("assert f2");

    let (ep_node, episode) = insert_episode(&store);
    let summary = note_with_id(
        Id::from_content_hash(b"alice-summary-bucket-0"),
        "Alice works on Aionforge and is based in NYC.",
        &episode.identity.id,
    );
    let mut artifacts = ConsolidationArtifacts::default();
    artifacts.notes.push(MaterializedNote {
        note: summary.clone(),
        source_facts: vec![
            fact_key(
                &subject_id,
                "works_on",
                ObjectValue::Text("Aionforge".to_string()),
            ),
            fact_key(
                &subject_id,
                "based_in",
                ObjectValue::Text("NYC".to_string()),
            ),
        ],
    });

    commit(&store, ep_node, &episode, &artifacts);

    assert_eq!(
        note_count_by_id(&store, &summary.identity.id),
        1,
        "the summary note is written"
    );
    assert_eq!(
        note_lineage_edges(&store),
        2,
        "one DERIVED_FROM edge per source fact"
    );
    // The source facts are untouched — summarization is non-lossy.
    assert_eq!(fact_count_by_id(&store, &f1.identity.id), 1);
    assert_eq!(fact_count_by_id(&store, &f2.identity.id), 1);

    // Replay the same artifacts: the note id and the edges already exist, so nothing new.
    reset_to_raw(&store);
    commit(&store, ep_node, &episode, &artifacts);
    assert_eq!(
        note_count_by_id(&store, &summary.identity.id),
        1,
        "replay writes no second note"
    );
    assert_eq!(
        note_lineage_edges(&store),
        2,
        "replay adds no second lineage edge"
    );
}

#[test]
fn materialize_note_resolves_a_source_fact_created_in_the_same_transaction() {
    // The note's source fact is asserted in the SAME artifacts (not yet committed), so it
    // must resolve via the in-transaction fact map, not the committed index.
    let store = store();
    let (subject_id, _subject_node) = insert_entity(&store, "Alice");
    let (ep_node, episode) = insert_episode(&store);

    let new_fact = fact(
        &subject_id,
        "works_on",
        ObjectValue::Text("Aionforge".to_string()),
        "Alice works on Aionforge",
    );
    let summary = note_with_id(
        Id::from_content_hash(b"alice-summary-same-txn"),
        "Alice works on Aionforge.",
        &episode.identity.id,
    );
    let mut artifacts = ConsolidationArtifacts::default();
    artifacts.facts.push(MaterializedFact {
        fact: new_fact.clone(),
        about: open_window("2026-06-06T11:00:00Z[UTC]"),
    });
    artifacts.notes.push(MaterializedNote {
        note: summary.clone(),
        source_facts: vec![fact_key(
            &subject_id,
            "works_on",
            ObjectValue::Text("Aionforge".to_string()),
        )],
    });

    commit(&store, ep_node, &episode, &artifacts);

    assert_eq!(note_count_by_id(&store, &summary.identity.id), 1);
    assert_eq!(
        note_lineage_edges(&store),
        1,
        "the in-transaction source fact resolves and is wired"
    );
}

#[test]
fn materialize_note_skips_an_unresolvable_source_without_failing_the_commit() {
    // A source-fact key that names no committed or in-transaction fact is a pass bug;
    // materialization degrades gracefully — the note is still written, the bad lineage
    // edge is dropped (logged), and the commit succeeds.
    let store = store();
    let (subject_id, _subject_node) = insert_entity(&store, "Alice");
    let (ep_node, episode) = insert_episode(&store);

    let summary = note_with_id(
        Id::from_content_hash(b"alice-summary-orphan"),
        "A summary whose source cannot be resolved.",
        &episode.identity.id,
    );
    let mut artifacts = ConsolidationArtifacts::default();
    artifacts.notes.push(MaterializedNote {
        note: summary.clone(),
        source_facts: vec![fact_key(
            &subject_id,
            "works_on",
            ObjectValue::Text("Nowhere".to_string()),
        )],
    });

    commit(&store, ep_node, &episode, &artifacts);

    assert_eq!(
        note_count_by_id(&store, &summary.identity.id),
        1,
        "the note is still written"
    );
    assert_eq!(
        note_lineage_edges(&store),
        0,
        "the unresolvable source wrote no lineage edge"
    );
}
