//! Store-level tests for the off-cursor LLM-distillation write surface (M3.T08):
//! `materialize_distilled_notes` writes content-addressed `Note`s and their
//! `Note -DERIVED_FROM-> Fact` lineage, wires each written note's `distill`
//! `AuditEvent -AUDIT-> Note` provenance edge (and a declined call's audit `-AUDIT-> Entity`),
//! in its own transaction, idempotently, and without ever touching an episode or the
//! consolidation cursor.

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
    BoundQuery, DistilledNoteWrite, FactKey, MaterializedNote, NodeId, QueryResult, Store,
    StoreConfig, Value,
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

fn assert_fact(store: &Store, f: &Fact, subject_node: NodeId) {
    store
        .assert_fact(f, subject_node, &open_window("2026-06-06T09:00:00Z[UTC]"))
        .expect("assert fact");
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
        subject_id: *subject_id,
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
fn distill_audit(id: Id, subject_id: &Id, outcome: &str) -> AuditEvent {
    AuditEvent {
        identity: identity(id),
        kind: AuditKind::Distill,
        subject_id: *subject_id,
        actor_id: Id::from_content_hash(b"distiller/llm-distill-v1"),
        payload: serde_json::json!({
            "outcome": outcome,
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
        .bind_uuid("id", id)
        .expect("bind id");
    match store.execute(&query).expect("note count") {
        QueryResult::Rows(rows) => rows.row_count(),
        _ => 0,
    }
}

fn audit_count_by_id(store: &Store, id: &Id) -> usize {
    let query = BoundQuery::new("MATCH (a:AuditEvent) WHERE a.id = $id RETURN a.id AS id")
        .bind_uuid("id", id)
        .expect("bind id");
    match store.execute(&query).expect("audit count") {
        QueryResult::Rows(rows) => rows.row_count(),
        _ => 0,
    }
}

/// The id of the `Note` a given `distill` audit is wired to (its provenance target), if any.
fn note_id_for_audit(store: &Store, audit_id: &Id) -> Option<String> {
    let query = BoundQuery::new(
        "MATCH (a:AuditEvent)-[:AUDIT]->(n:Note) WHERE a.id = $id RETURN n.id AS id",
    )
    .bind_uuid("id", audit_id)
    .expect("bind id");
    match store.execute(&query).expect("audit->note") {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Uuid(u)) => Some(u.to_string()),
            _ => None,
        },
        _ => None,
    }
}

fn count(store: &Store, pattern: &str) -> u64 {
    let query = BoundQuery::new(pattern);
    match store.execute(&query).expect("count") {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Uint(n)) => *n,
            Some(Value::Int(n)) => u64::try_from(*n).unwrap_or(0),
            _ => 0,
        },
        _ => 0,
    }
}

fn lineage_edges(store: &Store) -> u64 {
    count(
        store,
        "MATCH (:Note)-[r:DERIVED_FROM]->(:Fact) RETURN count(r) AS n",
    )
}

fn audit_to_note_edges(store: &Store) -> u64 {
    count(
        store,
        "MATCH (:AuditEvent)-[r:AUDIT]->(:Note) RETURN count(r) AS n",
    )
}

fn audit_to_entity_edges(store: &Store) -> u64 {
    count(
        store,
        "MATCH (:AuditEvent)-[r:AUDIT]->(:Entity) RETURN count(r) AS n",
    )
}

fn fact_count_by_id(store: &Store, id: &Id) -> usize {
    let query = BoundQuery::new("MATCH (f:Fact) WHERE f.id = $id RETURN f.id AS id")
        .bind_uuid("id", id)
        .expect("bind id");
    match store.execute(&query).expect("fact count") {
        QueryResult::Rows(rows) => rows.row_count(),
        _ => 0,
    }
}

#[test]
fn a_written_note_carries_lineage_and_audit_to_note_provenance_idempotently() {
    let store = store();
    let (subject_id, subject_node) = insert_entity(&store, "Alice");

    // Two committed facts the distilled note rolls up.
    let f1 = fact(
        &subject_id,
        "works_on",
        ObjectValue::Text("Aionforge".to_string()),
        "Alice works on Aionforge",
    );
    assert_fact(&store, &f1, subject_node);
    let f2 = fact(
        &subject_id,
        "based_in",
        ObjectValue::Text("NYC".to_string()),
        "Alice is based in NYC",
    );
    assert_fact(&store, &f2, subject_node);

    let note = distilled_note(
        Id::from_content_hash(b"alice-distill-llm-distill-v1"),
        "Alice works on Aionforge and is based in NYC.",
    );
    let audit_id = Id::from_content_hash(b"alice-distill-audit");
    let written = vec![DistilledNoteWrite {
        note: MaterializedNote {
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
        },
        audit: distill_audit(audit_id, &subject_id, "written"),
    }];

    store
        .materialize_distilled_notes(&written, &[], &now())
        .expect("materialize distilled notes");

    assert_eq!(note_count_by_id(&store, &note.identity.id), 1);
    assert_eq!(lineage_edges(&store), 2, "one DERIVED_FROM per source fact");
    assert_eq!(audit_count_by_id(&store, &audit_id), 1);
    assert_eq!(
        audit_to_note_edges(&store),
        1,
        "provenance is wired audit -> the note it produced"
    );
    assert_eq!(
        audit_to_entity_edges(&store),
        0,
        "a written note is not anchored on an entity"
    );
    assert_eq!(
        note_id_for_audit(&store, &audit_id),
        Some(note.identity.id.to_string()),
        "the audit points to its own note (single-hop provenance for the cross-family guard)"
    );
    // The canonical source facts are untouched — distillation is non-lossy and non-canonical.
    assert_eq!(fact_count_by_id(&store, &f1.identity.id), 1);
    assert_eq!(fact_count_by_id(&store, &f2.identity.id), 1);

    // Replay the same batch: every id already exists, so nothing new is written.
    store
        .materialize_distilled_notes(&written, &[], &now())
        .expect("replay distilled notes");
    assert_eq!(
        note_count_by_id(&store, &note.identity.id),
        1,
        "no second note"
    );
    assert_eq!(lineage_edges(&store), 2, "no second lineage edge");
    assert_eq!(audit_count_by_id(&store, &audit_id), 1, "no second audit");
    assert_eq!(audit_to_note_edges(&store), 1, "no second provenance edge");
}

#[test]
fn a_multi_note_batch_wires_each_audit_to_its_own_note() {
    let store = store();
    let (alice_id, alice_node) = insert_entity(&store, "Alice");
    let (bob_id, bob_node) = insert_entity(&store, "Bob");
    let fa = fact(
        &alice_id,
        "works_on",
        ObjectValue::Text("Aionforge".to_string()),
        "Alice works on Aionforge",
    );
    assert_fact(&store, &fa, alice_node);
    let fb = fact(
        &bob_id,
        "works_on",
        ObjectValue::Text("Aionforge".to_string()),
        "Bob works on Aionforge",
    );
    assert_fact(&store, &fb, bob_node);

    let alice_note = distilled_note(
        Id::from_content_hash(b"alice-note"),
        "Alice works on Aionforge.",
    );
    let bob_note = distilled_note(
        Id::from_content_hash(b"bob-note"),
        "Bob works on Aionforge.",
    );
    let alice_audit = Id::from_content_hash(b"alice-audit");
    let bob_audit = Id::from_content_hash(b"bob-audit");
    let written = vec![
        DistilledNoteWrite {
            note: MaterializedNote {
                note: alice_note.clone(),
                source_facts: vec![fact_key(
                    &alice_id,
                    "works_on",
                    ObjectValue::Text("Aionforge".to_string()),
                )],
            },
            audit: distill_audit(alice_audit, &alice_id, "written"),
        },
        DistilledNoteWrite {
            note: MaterializedNote {
                note: bob_note.clone(),
                source_facts: vec![fact_key(
                    &bob_id,
                    "works_on",
                    ObjectValue::Text("Aionforge".to_string()),
                )],
            },
            audit: distill_audit(bob_audit, &bob_id, "written"),
        },
    ];

    store
        .materialize_distilled_notes(&written, &[], &now())
        .expect("materialize multi-note batch");

    assert_eq!(
        audit_to_note_edges(&store),
        2,
        "one provenance edge per note"
    );
    assert_eq!(
        note_id_for_audit(&store, &alice_audit),
        Some(alice_note.identity.id.to_string()),
        "alice's audit points to alice's note, not bob's"
    );
    assert_eq!(
        note_id_for_audit(&store, &bob_audit),
        Some(bob_note.identity.id.to_string()),
        "bob's audit points to bob's note"
    );

    // Idempotent across the whole batch.
    store
        .materialize_distilled_notes(&written, &[], &now())
        .expect("replay multi-note batch");
    assert_eq!(
        audit_to_note_edges(&store),
        2,
        "replay adds no provenance edge"
    );
    assert_eq!(lineage_edges(&store), 2, "replay adds no lineage edge");
}

#[test]
fn a_note_with_an_unresolvable_source_is_written_with_the_edge_dropped() {
    // The note's source fact is not committed, so it cannot resolve; materialization degrades
    // gracefully — the note is still written, the bad lineage edge is dropped (logged), and the
    // provenance audit is still wired to the note.
    let store = store();
    let (subject_id, _subject_node) = insert_entity(&store, "Alice");

    let note = distilled_note(
        Id::from_content_hash(b"alice-distill-orphan"),
        "A distilled note whose source cannot be resolved.",
    );
    let audit_id = Id::from_content_hash(b"alice-distill-orphan-audit");
    let written = vec![DistilledNoteWrite {
        note: MaterializedNote {
            note: note.clone(),
            source_facts: vec![fact_key(
                &subject_id,
                "works_on",
                ObjectValue::Text("Nowhere".to_string()),
            )],
        },
        audit: distill_audit(audit_id, &subject_id, "written"),
    }];

    store
        .materialize_distilled_notes(&written, &[], &now())
        .expect("materialize distilled notes");

    assert_eq!(
        note_count_by_id(&store, &note.identity.id),
        1,
        "the note is still written"
    );
    assert_eq!(
        lineage_edges(&store),
        0,
        "the unresolvable source wrote no lineage edge"
    );
    assert_eq!(
        audit_to_note_edges(&store),
        1,
        "the provenance audit is still wired to the note"
    );
}

#[test]
fn a_declined_call_is_audited_and_anchored_on_its_subject_entity() {
    // A call the detail-retention guard rejected (or the distiller declined) produced no note,
    // but is still audited "for every call" — anchored on the cluster subject entity.
    let store = store();
    let (subject_id, _subject_node) = insert_entity(&store, "Alice");
    let audit_id = Id::from_content_hash(b"alice-declined-audit");
    let declined = vec![distill_audit(audit_id, &subject_id, "rejected_lossy")];

    store
        .materialize_distilled_notes(&[], &declined, &now())
        .expect("materialize declined audit");

    assert_eq!(
        audit_count_by_id(&store, &audit_id),
        1,
        "the declined call is audited"
    );
    assert_eq!(
        audit_to_entity_edges(&store),
        1,
        "anchored on its subject entity"
    );
    assert_eq!(
        audit_to_note_edges(&store),
        0,
        "a declined call wires no note"
    );
}

#[test]
fn a_declined_audit_with_an_absent_subject_still_records_without_an_edge() {
    let store = store();
    let absent_subject = Id::from_content_hash(b"never-committed-entity");
    let audit_id = Id::from_content_hash(b"orphan-declined-audit");
    let declined = vec![distill_audit(audit_id, &absent_subject, "declined")];

    store
        .materialize_distilled_notes(&[], &declined, &now())
        .expect("materialize audit-only batch");

    assert_eq!(
        audit_count_by_id(&store, &audit_id),
        1,
        "the audit node is still written"
    );
    assert_eq!(
        audit_to_entity_edges(&store),
        0,
        "no edge to the absent subject"
    );
}

#[test]
fn an_empty_batch_is_a_no_op() {
    let store = store();
    store
        .materialize_distilled_notes(&[], &[], &now())
        .expect("empty batch succeeds");
    assert_eq!(lineage_edges(&store), 0);
    assert_eq!(audit_to_note_edges(&store), 0);
    assert_eq!(audit_to_entity_edges(&store), 0);
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
    assert_fact(&store, &f1, subject_node);

    let note = distilled_note(
        Id::from_content_hash(b"alice-distill-no-episode"),
        "Alice works on Aionforge.",
    );
    let written = vec![DistilledNoteWrite {
        note: MaterializedNote {
            note,
            source_facts: vec![fact_key(
                &subject_id,
                "works_on",
                ObjectValue::Text("Aionforge".to_string()),
            )],
        },
        audit: distill_audit(
            Id::from_content_hash(b"alice-distill-no-episode-audit"),
            &subject_id,
            "written",
        ),
    }];

    store
        .materialize_distilled_notes(&written, &[], &now())
        .expect("materialize distilled notes");

    assert_eq!(
        count(&store, "MATCH (e:Episode) RETURN count(e) AS n"),
        0,
        "distillation creates no episode"
    );
    assert_eq!(
        count(&store, "MATCH (c:ConsolidationCursor) RETURN count(c) AS n"),
        0,
        "distillation advances no cursor"
    );
}
