//! Acceptance tests for native index and candidate-state-provider registration.
//!
//! Pins the data-model §7–§9 / §13.5 contract: the migration registers the vector,
//! text, scalar, and composite indexes (idempotently); the dimension-consistency check
//! fails loudly on a mismatch; and the candidate-state providers are registered and
//! track Fact membership through edge changes.

use aionforge_domain::blocks::Identity;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_store::{
    BoundQuery, DEFAULT_EMBEDDING_DIMENSION, QueryResult, Store, StoreConfig, Value,
};

use jiff::Zoned;

/// A stable [`Id`] for a string tag, so the synthetic id columns (now UUID-typed) still
/// join the same way: the same tag always maps to the same UUID within and across queries.
fn tag_id(tag: &str) -> Id {
    Id::from_content_hash(tag.as_bytes())
}

fn now() -> Zoned {
    "2026-06-06T12:00:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn zdt() -> Value {
    Value::ZonedDateTime(Box::new(now()))
}

fn migrated() -> Store {
    Store::open_in_memory_migrated(&now()).expect("open and migrate")
}

#[test]
fn migration_registers_all_native_indexes() {
    let store = migrated();

    // §7: one HNSW/cosine vector index per embedding property, pinned at the configured
    // dimension.
    let vectors = store.vector_indexes();
    assert_eq!(vectors.len(), 7, "vector index count: {vectors:?}");
    for vector in &vectors {
        assert_eq!(vector.kind, "HnswCosine", "{} is HNSW/cosine", vector.label);
        assert_eq!(
            vector.dimension, DEFAULT_EMBEDDING_DIMENSION,
            "{} pinned at the configured dimension",
            vector.label
        );
    }
    assert!(
        vectors
            .iter()
            .any(|v| v.label == "Skill" && v.property == "problem_embedding_v1"),
        "Skill indexes its problem embedding"
    );

    // §8: BM25 text indexes over the five content surfaces.
    assert_eq!(store.text_indexes().len(), 5, "text index count");

    // §8 + §11: scalar property indexes — namespace on every kind (17) plus the 32 per-kind
    // INDEXED entries in SCALAR_INDEXES (which include Entity.id, Note.id, AuditEvent.id for
    // consolidation resolution and audit dedup, Skill.id for the by-domain-id procedural
    // lookups, Agent.id for provenance key resolution, Episode.id for the signed-write
    // collision pre-check (M4.T03), Fact.id for the quorum-promotion global-copy
    // idempotency probe (M4.T04), and AuditEvent.actor_id + AuditEvent.occurred_at (the
    // first datetime property index) for the M4.T06 audit-history readers) = 49.
    assert_eq!(
        store.property_indexes().len(),
        49,
        "scalar property index count"
    );

    // §8: the three pure-scalar composites plus the two AuditEvent temporal composites
    // (now that selene indexes ZONED DATETIME).
    let composites = store.composite_indexes();
    assert_eq!(composites.len(), 5, "composite index count: {composites:?}");
    assert!(
        composites
            .iter()
            .any(|(label, cols)| label == "Skill" && cols == &["name", "version"]),
        "Skill(name, version) composite present"
    );
    assert!(
        composites
            .iter()
            .any(|(label, cols)| label == "AuditEvent" && cols == &["subject_id", "occurred_at"]),
        "AuditEvent(subject_id, occurred_at) temporal composite present"
    );
    assert!(
        composites
            .iter()
            .any(|(label, cols)| label == "AuditEvent" && cols == &["kind", "occurred_at"]),
        "AuditEvent(kind, occurred_at) temporal composite present"
    );

    // §8: occurred_at is the first ZONED DATETIME property index in the schema.
    assert!(
        store
            .property_indexes()
            .iter()
            .any(|(label, prop)| label == "AuditEvent" && prop == "occurred_at"),
        "AuditEvent.occurred_at datetime property index present"
    );
}

#[test]
fn dimension_consistency_check_passes_and_fails() {
    let store = migrated();
    // Matches the dimension the indexes were created at.
    store
        .dimension_consistency_check(DEFAULT_EMBEDDING_DIMENSION)
        .expect("matching dimension passes");
    // A different embedder dimension must fail loudly (§13.5).
    let mismatch = store.dimension_consistency_check(768);
    assert!(mismatch.is_err(), "mismatched dimension must fail");
}

#[test]
fn index_registration_is_idempotent() {
    let store = migrated();
    let before = (
        store.vector_indexes().len(),
        store.text_indexes().len(),
        store.property_indexes().len(),
        store.composite_indexes().len(),
    );

    // A second migrate is a no-op (version guard), and even a forced re-registration
    // would skip existing indexes — re-running changes nothing.
    let report = store.migrate(&now()).expect("second migrate");
    assert!(report.is_noop(), "second migrate is a no-op: {report:?}");

    let after = (
        store.vector_indexes().len(),
        store.text_indexes().len(),
        store.property_indexes().len(),
        store.composite_indexes().len(),
    );
    assert_eq!(before, after, "index counts stable across re-migration");
}

/// An `AuditEvent` keyed by `marker` with a specific `occurred_at`, for the temporal index.
fn audit_at(marker: &str, occurred: &str) -> AuditEvent {
    let when: Zoned = occurred.parse().expect("valid zoned datetime");
    AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(marker.as_bytes()),
            ingested_at: now(),
            namespace: Namespace::System,
            expired_at: None,
        },
        kind: AuditKind::Promote,
        subject_id: Id::from_content_hash(b"audit-subject"),
        actor_id: Id::from_content_hash(b"substrate"),
        payload: serde_json::json!({ "marker": marker }),
        signature: String::new(),
        occurred_at: when,
    }
}

/// The `a.id` UUID column of a query, in row order.
fn id_column(store: &Store, query: &BoundQuery) -> Vec<String> {
    match store.execute(query).expect("query audit ids") {
        QueryResult::Rows(rows) => (0..rows.row_count())
            .map(|r| match rows.value(r, 0) {
                Some(Value::Uuid(u)) => u.to_string(),
                other => panic!("id was not a uuid: {other:?}"),
            })
            .collect(),
        other => panic!("unexpected query result: {other:?}"),
    }
}

#[test]
fn audit_events_filter_on_the_occurred_at_datetime_index() {
    let store = migrated();

    // `occurred_at` is the first ZONED DATETIME property index in the schema. Events at distinct
    // instants are committed OUT of chronological order, then a half-open `[t1, t3)` range filter
    // over the index must select exactly the in-range set — exercising the STRICT type-check of
    // `occurred_at` through the real `to_node` write path and the substrate's datetime comparison.
    let t1 = "2026-06-06T08:00:00-05:00[America/Chicago]"; // 13:00 UTC
    let t2 = "2026-06-06T09:00:00-05:00[America/Chicago]"; // 14:00 UTC
    let t3 = "2026-06-06T10:00:00-05:00[America/Chicago]"; // 15:00 UTC
    // The discriminator: same instant of 13:30 UTC, but written in a zone whose lexical string
    // ("...T13:30...+00:00") sorts ABOVE `hi`'s ("...T10:00...-05:00"). A string comparison would
    // wrongly exclude it from `< hi`; only a true instant comparison keeps it (13:30 < 15:00 UTC).
    let tz = "2026-06-06T13:30:00+00:00[UTC]";
    store.commit_audit(&audit_at("evt-2", t2)).expect("t2");
    store.commit_audit(&audit_at("evt-1", t1)).expect("t1");
    store.commit_audit(&audit_at("evt-3", t3)).expect("t3");
    store.commit_audit(&audit_at("evt-tz", tz)).expect("tz");

    // Result as a set (sorted in Rust) — ordering is the reader's job (PR-2 sorts the bounded
    // result by `(occurred_at, id)` in Rust rather than leaning on GQL ORDER BY).
    let lo: Zoned = t1.parse().unwrap();
    let hi: Zoned = t3.parse().unwrap();
    let query = BoundQuery::new(
        "MATCH (a:AuditEvent) \
         WHERE a.occurred_at >= $lo AND a.occurred_at < $hi \
         RETURN a.id",
    )
    .bind("lo", Value::ZonedDateTime(Box::new(lo)))
    .unwrap()
    .bind("hi", Value::ZonedDateTime(Box::new(hi)))
    .unwrap();

    let mut got = id_column(&store, &query);
    got.sort();
    // evt-1 (lower bound, inclusive), evt-2, and evt-tz (kept only under instant comparison);
    // evt-3 is excluded at the open upper bound.
    let mut want = vec![
        Id::from_content_hash(b"evt-1").to_string(),
        Id::from_content_hash(b"evt-2").to_string(),
        Id::from_content_hash(b"evt-tz").to_string(),
    ];
    want.sort();
    assert_eq!(
        got, want,
        "the range filter selects the in-range events by instant, not by lexical string"
    );
}

#[test]
fn custom_dimension_is_pinned_into_the_vector_indexes() {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 768,
    })
    .expect("open store");
    store.migrate(&now()).expect("migrate");

    assert!(
        store.vector_indexes().iter().all(|v| v.dimension == 768),
        "vector indexes use the configured dimension"
    );
    store
        .dimension_consistency_check(768)
        .expect("check against the configured dimension");
    assert!(store.dimension_consistency_check(1536).is_err());
}

/// Insert a valid Fact via bound parameters. Every required field is bound except
/// `is_pinned`, which is left to its schema `DEFAULT FALSE`; `status` is bound to its
/// default `'active'` explicitly because the provider membership downstream depends on it.
fn insert_fact(store: &Store, id: &str, subject: &str) {
    let query = BoundQuery::new(
        "INSERT (f:Fact {id: $id, ingested_at: $ts, namespace: $ns, importance: $imp, \
         trust: $tr, last_access: $ts, access_count_recent: $ac, referenced_count: $rc, \
         surprise: $su, subject_id: $subj, predicate: $pred, object_kind: $ok, \
         confidence: $conf, status: $st, statement: $stmt})",
    )
    .bind_uuid("id", tag_id(id))
    .unwrap()
    .bind("ts", zdt())
    .unwrap()
    .bind_str("ns", "agent:test")
    .unwrap()
    .bind("imp", Value::Float(0.5))
    .unwrap()
    .bind("tr", Value::Float(0.5))
    .unwrap()
    .bind("ac", Value::Uint(0))
    .unwrap()
    .bind("rc", Value::Uint(0))
    .unwrap()
    .bind("su", Value::Float(0.0))
    .unwrap()
    .bind_uuid("subj", tag_id(subject))
    .unwrap()
    .bind_str("pred", "relates_to")
    .unwrap()
    .bind_str("ok", "string")
    .unwrap()
    .bind("conf", Value::Float(0.9))
    .unwrap()
    .bind_str("st", "active")
    .unwrap()
    .bind_str("stmt", "a canonical statement")
    .unwrap();
    store.execute(&query).expect("insert fact");
}

fn count(store: &Store, name: &str) -> usize {
    store
        .candidate_state_infos()
        .expect("candidate-state infos")
        .into_iter()
        .find(|info| info.name == name)
        .unwrap_or_else(|| panic!("provider {name} is registered"))
        .candidate_count
}

#[test]
fn candidate_state_providers_are_registered() {
    let store = migrated();
    let names: Vec<String> = store
        .candidate_state_infos()
        .expect("infos")
        .into_iter()
        .map(|info| info.name)
        .collect();

    for expected in [
        "current_support_facts",
        "provenance_current_support_facts",
        "scope_membership",
        "recency_active",
        "unresolved_current",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "{expected} registered"
        );
    }
    // Nothing is a member on a freshly migrated graph (no Facts, no edges).
    assert_eq!(count(&store, "current_support_facts"), 0);
}

#[test]
fn current_support_facts_tracks_fact_membership() {
    let store = migrated();
    insert_fact(&store, "fact-1", "entity-a");
    insert_fact(&store, "fact-2", "entity-b");

    // Both fresh Facts are current support; neither has provenance grounding.
    assert_eq!(count(&store, "current_support_facts"), 2);
    assert_eq!(count(&store, "unresolved_current"), 2);
    assert_eq!(count(&store, "provenance_current_support_facts"), 0);
    assert_eq!(count(&store, "scope_membership"), 0);

    // Supersede fact-1 with fact-2: fact-1 gains an outgoing SUPERSEDED_BY and drops
    // out of the current-support set (exclude_outgoing), leaving only fact-2.
    let supersede = BoundQuery::new(
        "MATCH (a:Fact {id: $from}), (b:Fact {id: $to}) \
         INSERT (a)-[:SUPERSEDED_BY {valid_from: $ts, ingested_at: $ts, reason: $reason}]->(b)",
    )
    .bind_uuid("from", tag_id("fact-1"))
    .unwrap()
    .bind_uuid("to", tag_id("fact-2"))
    .unwrap()
    .bind("ts", zdt())
    .unwrap()
    .bind_str("reason", "superseded by a newer assertion")
    .unwrap();
    store.execute(&supersede).expect("supersede");

    assert_eq!(count(&store, "current_support_facts"), 1);
    // unresolved_current ignores SUPERSEDED_BY (it only excludes CONTRADICTS).
    assert_eq!(count(&store, "unresolved_current"), 2);

    // A new fact contradicting the incumbent: fact-3 -[CONTRADICTS]-> fact-2. The
    // CONTRADICTS source (fact-3, outgoing) is the quarantined one and drops out of the
    // current-support set; fact-2 (the incumbent, incoming) stays. This pins the
    // exclude_outgoing(CONTRADICTS) direction.
    insert_fact(&store, "fact-3", "entity-b");
    assert_eq!(
        count(&store, "current_support_facts"),
        2,
        "fact-2 and fact-3"
    );
    let contradict = BoundQuery::new(
        "MATCH (a:Fact {id: $from}), (b:Fact {id: $to}) \
         INSERT (a)-[:CONTRADICTS {valid_from: $ts, ingested_at: $ts, detected_by: $by}]->(b)",
    )
    .bind_uuid("from", tag_id("fact-3"))
    .unwrap()
    .bind_uuid("to", tag_id("fact-2"))
    .unwrap()
    .bind("ts", zdt())
    .unwrap()
    .bind_str("by", "contradiction-detector")
    .unwrap();
    store.execute(&contradict).expect("contradict");

    // fact-3 (the contradicting source) drops out; fact-2 (incumbent) remains.
    assert_eq!(
        count(&store, "current_support_facts"),
        1,
        "only fact-2 remains"
    );

    // unresolved_current is the dual direction: it drops the contradiction *target*
    // (fact-2, the contested incumbent, incoming CONTRADICTS) and keeps the source and
    // the uncontested fact. So current_support_facts minus unresolved_current isolates
    // exactly the contested incumbent (fact-2) — the §9 quarantine-reasoning use.
    assert_eq!(
        count(&store, "unresolved_current"),
        2,
        "fact-1 and fact-3 are uncontested; fact-2 (incoming CONTRADICTS) drops out"
    );
}

/// Insert a minimal valid ProvenanceRecord (every NOT NULL field bound).
fn insert_provenance(store: &Store, id: &str, subject: &str) {
    let query = BoundQuery::new(
        "INSERT (p:ProvenanceRecord {id: $id, ingested_at: $ts, namespace: $ns, \
         subject_id: $subj, writer_agent_id: $writer, signature: $sig, \
         model_family: $mf, trust_at_write: $tw})",
    )
    .bind_uuid("id", tag_id(id))
    .unwrap()
    .bind("ts", zdt())
    .unwrap()
    .bind_str("ns", "agent:test")
    .unwrap()
    .bind_uuid("subj", tag_id(subject))
    .unwrap()
    .bind_uuid("writer", tag_id("agent:test"))
    .unwrap()
    .bind_str("sig", "signature-bytes")
    .unwrap()
    .bind_str("mf", "test-model")
    .unwrap()
    .bind("tw", Value::Float(0.5))
    .unwrap();
    store.execute(&query).expect("insert provenance record");
}

/// Insert a minimal valid Scope (every NOT NULL field bound).
fn insert_scope(store: &Store, id: &str) {
    let query = BoundQuery::new(
        "INSERT (s:Scope {id: $id, ingested_at: $ts, namespace: $ns, \
         name: $name, scope_kind: $kind})",
    )
    .bind_uuid("id", tag_id(id))
    .unwrap()
    .bind("ts", zdt())
    .unwrap()
    .bind_str("ns", "agent:test")
    .unwrap()
    .bind_str("name", "test-scope")
    .unwrap()
    .bind_str("kind", "task")
    .unwrap();
    store.execute(&query).expect("insert scope");
}

/// Insert a minimal valid RecencyWindow (every NOT NULL field bound).
fn insert_recency_window(store: &Store, id: &str) {
    let query = BoundQuery::new(
        "INSERT (w:RecencyWindow {id: $id, ingested_at: $ts, namespace: $ns, label: $label})",
    )
    .bind_uuid("id", tag_id(id))
    .unwrap()
    .bind("ts", zdt())
    .unwrap()
    .bind_str("ns", "agent:test")
    .unwrap()
    .bind_str("label", "last-hour")
    .unwrap();
    store.execute(&query).expect("insert recency window");
}

#[test]
fn provenance_current_support_facts_requires_support_and_grounding() {
    let store = migrated();
    insert_fact(&store, "fact-1", "entity-a");
    insert_fact(&store, "fact-2", "entity-b");
    insert_provenance(&store, "prov-1", "fact-2");

    // Both facts are current support, but neither is grounded yet: the provider needs
    // both an incoming SUPPORTS and an outgoing HAS_PROVENANCE.
    assert_eq!(count(&store, "provenance_current_support_facts"), 0);

    // Give fact-2 an incoming SUPPORTS (fact-1 -[SUPPORTS]-> fact-2).
    let supports = BoundQuery::new(
        "MATCH (a:Fact {id: $from}), (b:Fact {id: $to}) \
         INSERT (a)-[:SUPPORTS {weight: $w}]->(b)",
    )
    .bind_uuid("from", tag_id("fact-1"))
    .unwrap()
    .bind_uuid("to", tag_id("fact-2"))
    .unwrap()
    .bind("w", Value::Float(1.0))
    .unwrap();
    store.execute(&supports).expect("supports");

    // An incoming SUPPORTS alone is not enough — the outgoing HAS_PROVENANCE is missing.
    assert_eq!(count(&store, "provenance_current_support_facts"), 0);

    // Ground fact-2: fact-2 -[HAS_PROVENANCE]-> prov-1.
    let grounds = BoundQuery::new(
        "MATCH (f:Fact {id: $fid}), (p:ProvenanceRecord {id: $pid}) \
         INSERT (f)-[:HAS_PROVENANCE]->(p)",
    )
    .bind_uuid("fid", tag_id("fact-2"))
    .unwrap()
    .bind_uuid("pid", tag_id("prov-1"))
    .unwrap();
    store.execute(&grounds).expect("has provenance");

    // fact-2 now satisfies the full rule; fact-1 has neither an incoming SUPPORTS nor a
    // grounding, so it stays out.
    assert_eq!(count(&store, "provenance_current_support_facts"), 1);
}

#[test]
fn scope_membership_tracks_in_scope_edges() {
    let store = migrated();
    insert_fact(&store, "fact-1", "entity-a");
    insert_scope(&store, "scope-1");
    assert_eq!(count(&store, "scope_membership"), 0);

    let in_scope = BoundQuery::new(
        "MATCH (f:Fact {id: $fid}), (s:Scope {id: $sid}) INSERT (f)-[:IN_SCOPE]->(s)",
    )
    .bind_uuid("fid", tag_id("fact-1"))
    .unwrap()
    .bind_uuid("sid", tag_id("scope-1"))
    .unwrap();
    store.execute(&in_scope).expect("in scope");

    assert_eq!(count(&store, "scope_membership"), 1);
}

#[test]
fn recency_active_tracks_recent_in_edges() {
    let store = migrated();
    insert_fact(&store, "fact-1", "entity-a");
    insert_recency_window(&store, "window-1");
    assert_eq!(count(&store, "recency_active"), 0);

    let recent_in = BoundQuery::new(
        "MATCH (f:Fact {id: $fid}), (w:RecencyWindow {id: $wid}) INSERT (f)-[:RECENT_IN]->(w)",
    )
    .bind_uuid("fid", tag_id("fact-1"))
    .unwrap()
    .bind_uuid("wid", tag_id("window-1"))
    .unwrap();
    store.execute(&recent_in).expect("recent in");

    assert_eq!(count(&store, "recency_active"), 1);
}
