//! Store-level tests for the off-cursor LLM-distillation write surface (M3.T08):
//! `materialize_distilled_notes` writes content-addressed `Note`s and their
//! `Note -DERIVED_FROM-> Fact` lineage plus a `distill` `AuditEvent -AUDIT-> Entity`
//! provenance edge, in its own transaction, idempotently, and without ever touching an
//! episode or the consolidation cursor.

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::edges::About;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::value::ObjectValue;
use aionforge_store::{
    BoundQuery, FactKey, MaterializedNote, NodeId, QueryResult, Store, StoreConfig, Value,
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
        identity: identity(id.clone()),
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
        subject_id: subject_id.clone(),
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

fn fact_key(subject_id: &Id, predicate: &str, object: ObjectValue) -> FactKey {
    FactKey {
        subject_id: subject_id.clone(),
        predicate: predicate.to_string(),
        object,
    }
}

/// A distilled note with an explicit content-addressed id (under the distiller's own rule
/// version, so it never collides with a rule summary's id-space).
fn distilled_note(id: Id, content: &str) -> Note {
    Note {
        identity: identity(id),
        stats: stats(),
        content: content.to_string(),
        context: None,
        keywords: vec!["alice".to_string(), "aionforge".to_string()],
        embedding: None,
        embedder_model: None,
        derived_from_episode: None,
    }
}

/// A `distill` audit recording one call's provenance: model identity, endpoint, seed, outcome.
fn distill_audit(id: Id, subject_id: &Id) -> AuditEvent {
    AuditEvent {
        identity: identity(id),
        kind: AuditKind::Distill,
        subject_id: subject_id.clone(),
        actor_id: Id::from_content_hash(b"distiller/llm-distill-v1"),
        payload: serde_json::json!({
            "outcome": "written",
            "model_family": "claude",
            "model_version": "opus-4-8",
            "endpoint": "https://api.anthropic.com/v1/messages",
            "seed": 42,
        }),
        signature: String::new(),
        occurred_at: now(),
    }
}

fn note_count_by_id(store: &Store, id: &Id) -> usize {
    let query = BoundQuery::new("MATCH (n:Note) WHERE n.id = $id RETURN n.id AS id")
        .bind_str("id", id.as_str())
        .expect("bind id");
    match store.execute(&query).expect("note count") {
        QueryResult::Rows(rows) => rows.row_count(),
        _ => 0,
    }
}

fn audit_count_by_id(store: &Store, id: &Id) -> usize {
    let query = BoundQuery::new("MATCH (a:AuditEvent) WHERE a.id = $id RETURN a.id AS id")
        .bind_str("id", id.as_str())
        .expect("bind id");
    match store.execute(&query).expect("audit count") {
        QueryResult::Rows(rows) => rows.row_count(),
        _ => 0,
    }
}

fn count_edges(store: &Store, pattern: &str) -> u64 {
    let query = BoundQuery::new(pattern);
    match store.execute(&query).expect("count edges") {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Uint(n)) => *n,
            Some(Value::Int(n)) => u64::try_from(*n).unwrap_or(0),
            _ => 0,
        },
        _ => 0,
    }
}

fn lineage_edges(store: &Store) -> u64 {
    count_edges(
        store,
        "MATCH (:Note)-[r:DERIVED_FROM]->(:Fact) RETURN count(r) AS n",
    )
}

fn audit_edges(store: &Store) -> u64 {
    count_edges(
        store,
        "MATCH (:AuditEvent)-[r:AUDIT]->(:Entity) RETURN count(r) AS n",
    )
}

fn fact_count_by_id(store: &Store, id: &Id) -> usize {
    let query = BoundQuery::new("MATCH (f:Fact) WHERE f.id = $id RETURN f.id AS id")
        .bind_str("id", id.as_str())
        .expect("bind id");
    match store.execute(&query).expect("fact count") {
        QueryResult::Rows(rows) => rows.row_count(),
        _ => 0,
    }
}

#[test]
fn distilled_notes_write_lineage_and_provenance_and_are_idempotent() {
    let store = store();
    let (subject_id, subject_node) = insert_entity(&store, "Alice");

    // Two committed facts the distilled note rolls up.
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

    let note = distilled_note(
        Id::from_content_hash(b"alice-distill-llm-distill-v1"),
        "Alice works on Aionforge and is based in NYC.",
    );
    let notes = vec![MaterializedNote {
        note: note.clone(),
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
    }];
    let audit_id = Id::from_content_hash(b"alice-distill-audit");
    let audits = vec![distill_audit(audit_id.clone(), &subject_id)];

    store
        .materialize_distilled_notes(&notes, &audits, &now())
        .expect("materialize distilled notes");

    assert_eq!(
        note_count_by_id(&store, &note.identity.id),
        1,
        "the distilled note is written"
    );
    assert_eq!(
        lineage_edges(&store),
        2,
        "one DERIVED_FROM edge per source fact"
    );
    assert_eq!(
        audit_count_by_id(&store, &audit_id),
        1,
        "the distill audit is written"
    );
    assert_eq!(
        audit_edges(&store),
        1,
        "the audit is wired to the subject entity it distilled"
    );
    // The canonical source facts are untouched — distillation is non-lossy and non-canonical.
    assert_eq!(fact_count_by_id(&store, &f1.identity.id), 1);
    assert_eq!(fact_count_by_id(&store, &f2.identity.id), 1);

    // Replay the same batch: every id already exists, so nothing new is written.
    store
        .materialize_distilled_notes(&notes, &audits, &now())
        .expect("replay distilled notes");
    assert_eq!(
        note_count_by_id(&store, &note.identity.id),
        1,
        "replay writes no second note"
    );
    assert_eq!(
        lineage_edges(&store),
        2,
        "replay adds no second lineage edge"
    );
    assert_eq!(
        audit_count_by_id(&store, &audit_id),
        1,
        "replay writes no second audit"
    );
    assert_eq!(audit_edges(&store), 1, "replay adds no second audit edge");
}

#[test]
fn a_distill_audit_with_an_absent_subject_still_records_without_an_edge() {
    // A rejected-lossy or declined call still happened, so it is audited even though it wrote
    // no note. If its subject entity is not in the committed graph, the audit node is still
    // written (the provenance is the payload) and only the AUDIT edge is skipped, logged.
    let store = store();
    let absent_subject = Id::from_content_hash(b"never-committed-entity");
    let audit_id = Id::from_content_hash(b"orphan-distill-audit");
    let audits = vec![distill_audit(audit_id.clone(), &absent_subject)];

    store
        .materialize_distilled_notes(&[], &audits, &now())
        .expect("materialize audit-only batch");

    assert_eq!(
        audit_count_by_id(&store, &audit_id),
        1,
        "the audit node is still written"
    );
    assert_eq!(
        audit_edges(&store),
        0,
        "no AUDIT edge is wired to the absent subject"
    );
}

#[test]
fn an_empty_batch_is_a_no_op() {
    let store = store();
    store
        .materialize_distilled_notes(&[], &[], &now())
        .expect("empty batch succeeds");
    assert_eq!(lineage_edges(&store), 0);
    assert_eq!(audit_edges(&store), 0);
}

#[test]
fn distillation_never_creates_an_episode_or_cursor() {
    // The off-cursor guarantee, asserted directly: distilling writes no episode and no
    // consolidation cursor singleton, so the scheduler's world is untouched.
    let store = store();
    let (subject_id, subject_node) = insert_entity(&store, "Alice");
    let f1 = fact(
        &subject_id,
        "works_on",
        ObjectValue::Text("Aionforge".to_string()),
        "Alice works on Aionforge",
    );
    store
        .assert_fact(&f1, subject_node, &open_window("2026-06-06T09:00:00Z[UTC]"))
        .expect("assert f1");

    let note = distilled_note(
        Id::from_content_hash(b"alice-distill-no-episode"),
        "Alice works on Aionforge.",
    );
    let notes = vec![MaterializedNote {
        note,
        source_facts: vec![fact_key(
            &subject_id,
            "works_on",
            ObjectValue::Text("Aionforge".to_string()),
        )],
    }];
    let audits = vec![distill_audit(
        Id::from_content_hash(b"alice-distill-no-episode-audit"),
        &subject_id,
    )];

    store
        .materialize_distilled_notes(&notes, &audits, &now())
        .expect("materialize distilled notes");

    assert_eq!(
        count_edges(&store, "MATCH (e:Episode) RETURN count(e) AS n"),
        0,
        "distillation creates no episode"
    );
    assert_eq!(
        count_edges(&store, "MATCH (c:ConsolidationCursor) RETURN count(c) AS n"),
        0,
        "distillation advances no cursor"
    );
}
