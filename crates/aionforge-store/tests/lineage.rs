//! Acceptance tests for the writer-identity and note-lineage reads (07 §3, M6.T01):
//! the fail-closed resolution chain (provenance record → origin → agent), the sticky
//! unverifiable flag, the distilled-note model union that closes the two-hop launder,
//! the startup family scan, and the lineage bundle.

mod common;

use std::collections::BTreeMap;

use common::{assert_about, entity, fact, identity, open_window, store, ts, zdt};

use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::nodes::agent::{Agent, AgentStatus, TrustScores};
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Origin, Role};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::semantic::Fact;
use aionforge_domain::value::ObjectValue;
use aionforge_store::{BoundQuery, NodeId, Store, Value};

const NOW: &str = "2026-06-10T09:00:00-05:00[America/Chicago]";
const FROM: &str = "2026-06-10T08:00:00-05:00[America/Chicago]";

/// Insert an `Episode` captured by `agent`, optionally carrying an `Origin` block.
fn insert_episode(store: &Store, agent: Id, seed: &[u8], origin: Option<Origin>) -> Id {
    let id = Id::generate();
    let episode = Episode {
        identity: identity(id),
        stats: common::stats(),
        content: "source".to_string(),
        role: Role::User,
        captured_at: ts(NOW),
        agent_id: agent,
        session_id: None,
        content_hash: ContentHash::of(seed),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin,
    };
    store.insert_episode(&episode).expect("insert episode");
    id
}

/// An `Origin` block declaring only the writer model family.
fn origin_with(family: Option<&str>) -> Origin {
    Origin {
        model_family: family.map(str::to_string),
        model_version: None,
        transport: None,
        request_id: None,
        redactions: Vec::new(),
        injection_flags: Vec::new(),
        capture_latency_ms: None,
    }
}

/// Wire `Episode -HAS_PROVENANCE-> ProvenanceRecord` with a chosen writer family.
fn provenance_with_family(store: &Store, episode_id: &Id, family: &str) {
    let record_id = Id::generate();
    let query = BoundQuery::new(
        "INSERT (p:ProvenanceRecord {id: $id, ingested_at: $ts, namespace: $ns, \
         subject_id: $subj, writer_agent_id: $writer, signature: $sig, \
         model_family: $mf, trust_at_write: $tw})",
    )
    .bind_uuid("id", record_id)
    .unwrap()
    .bind("ts", zdt())
    .unwrap()
    .bind_str("ns", "agent:alice")
    .unwrap()
    .bind_uuid("subj", episode_id)
    .unwrap()
    .bind_uuid("writer", Id::from_content_hash(b"agent:alice"))
    .unwrap()
    .bind_str("sig", "signature-bytes")
    .unwrap()
    .bind_str("mf", family)
    .unwrap()
    .bind("tw", Value::Float(0.5))
    .unwrap();
    store.execute(&query).expect("insert provenance record");
    let edge = BoundQuery::new(
        "MATCH (e:Episode {id: $from}), (p:ProvenanceRecord {id: $to}) \
         INSERT (e)-[:HAS_PROVENANCE]->(p)",
    )
    .bind_uuid("from", episode_id)
    .unwrap()
    .bind_uuid("to", record_id)
    .unwrap();
    store.execute(&edge).expect("wire provenance edge");
}

/// Wire a `DERIVED_FROM` edge from a `Fact` to a source `Episode`.
fn derive_fact(store: &Store, fact_id: &Id, episode_id: &Id) {
    let query = BoundQuery::new(
        "MATCH (a:Fact {id: $from}), (b:Episode {id: $to}) \
         INSERT (a)-[:DERIVED_FROM {derived_at: $ts}]->(b)",
    )
    .bind_uuid("from", fact_id)
    .unwrap()
    .bind_uuid("to", episode_id)
    .unwrap()
    .bind("ts", zdt())
    .unwrap();
    store.execute(&query).expect("derive fact edge");
}

/// Enroll an agent declaring `family`, returning its domain id.
fn enroll_agent(store: &Store, family: &str, seed: &[u8]) -> Id {
    let id = Id::from_content_hash(seed);
    let agent = Agent {
        identity: identity(id),
        public_key: "cHVibGljLWtleQ==".to_string(),
        model_family: family.to_string(),
        model_version: None,
        trust_scores: TrustScores(BTreeMap::new()),
        status: AgentStatus::Active,
    };
    store.create_agent(&agent).expect("enroll agent");
    id
}

/// Insert a bare `Note` and wire `Note -DERIVED_FROM-> Fact` lineage to each source.
fn insert_note(store: &Store, seed: &[u8], source_facts: &[&Id]) -> Id {
    let id = Id::from_content_hash(seed);
    let query = BoundQuery::new(
        "INSERT (n:Note {id: $id, ingested_at: $ts, namespace: $ns, importance: $f, \
         trust: $f, last_access: $ts, access_count_recent: $z, referenced_count: $z, \
         surprise: $s, is_pinned: false, content: $content})",
    )
    .bind_uuid("id", id)
    .unwrap()
    .bind("ts", zdt())
    .unwrap()
    .bind_str("ns", "agent:alice")
    .unwrap()
    .bind("f", Value::Float(0.5))
    .unwrap()
    .bind("z", Value::Uint(0))
    .unwrap()
    .bind("s", Value::Float(0.0))
    .unwrap()
    .bind_str("content", "a condensed note")
    .unwrap();
    store.execute(&query).expect("insert note");
    for fact_id in source_facts {
        let edge = BoundQuery::new(
            "MATCH (n:Note {id: $from}), (f:Fact {id: $to}) \
             INSERT (n)-[:DERIVED_FROM {derived_at: $ts}]->(f)",
        )
        .bind_uuid("from", id)
        .unwrap()
        .bind_uuid("to", *fact_id)
        .unwrap()
        .bind("ts", zdt())
        .unwrap();
        store.execute(&edge).expect("wire note lineage");
    }
    id
}

/// Commit a `Distill` audit event against a note, recording the distilling model.
fn distill_audit(store: &Store, note_id: &Id, family: Option<&str>, version: Option<&str>) {
    let event = AuditEvent {
        identity: identity(Id::from_content_hash(
            format!("distill|{note_id}|{family:?}").as_bytes(),
        )),
        kind: AuditKind::Distill,
        subject_id: *note_id,
        actor_id: Id::from_content_hash(b"distiller"),
        payload: serde_json::json!({
            "outcome": "written",
            "model_family": family,
            "model_version": version,
            "rule_version": "llm-distill-v1",
        }),
        signature: String::new(),
        occurred_at: ts(NOW),
    };
    store.commit_audit(&event).expect("commit distill audit");
}

/// Assert one fact about `subject`, returning its domain id and node.
fn seeded_fact(store: &Store, statement: &str) -> (Id, NodeId) {
    let subject = entity(statement);
    let f: Fact = fact(
        subject.identity.id,
        "concerns",
        ObjectValue::Text(statement.to_string()),
        statement,
    );
    let node = assert_about(store, &subject, &f, &open_window(FROM));
    (f.identity.id, node)
}

#[test]
fn the_signed_record_outranks_origin_and_agent() {
    let store = store();
    let agent = enroll_agent(&store, "agent-declared", b"agent-a");
    let (fact_id, _) = seeded_fact(&store, "the record wins");
    let episode = insert_episode(
        &store,
        agent,
        b"ep-record",
        Some(origin_with(Some("origin-copy"))),
    );
    provenance_with_family(&store, &episode, "claude-sonnet-4-6");
    derive_fact(&store, &fact_id, &episode);

    let set = store
        .writer_families_for_facts(&[fact_id])
        .expect("writer families");
    assert_eq!(
        set.families,
        vec!["claude-sonnet-4-6".to_string()],
        "the provenance record's family is final; origin and agent are not consulted"
    );
    assert!(!set.unverifiable, "a resolved family vouches");
}

#[test]
fn origin_then_agent_fill_in_when_no_record_exists() {
    let store = store();
    let agent = enroll_agent(&store, "mistral-large", b"agent-b");
    let (fact_id, _) = seeded_fact(&store, "fallbacks resolve in order");
    // No provenance record; the origin copy carries the family.
    let with_origin = insert_episode(
        &store,
        agent,
        b"ep-origin",
        Some(origin_with(Some("gemini-3"))),
    );
    derive_fact(&store, &fact_id, &with_origin);
    // No record, an origin block with no family: the agent's declaration is last.
    let with_agent = insert_episode(&store, agent, b"ep-agent", Some(origin_with(None)));
    derive_fact(&store, &fact_id, &with_agent);

    let set = store
        .writer_families_for_facts(&[fact_id])
        .expect("writer families");
    assert_eq!(
        set.families,
        vec!["gemini-3".to_string(), "mistral-large".to_string()],
        "each episode resolves through its own chain; the set is distinct and sorted"
    );
    assert!(!set.unverifiable);
}

#[test]
fn unresolvable_sources_are_unverifiable_never_dropped() {
    let store = store();
    // A fact with no source episode at all.
    let (orphan, _) = seeded_fact(&store, "an unsourced claim");
    let set = store
        .writer_families_for_facts(&[orphan])
        .expect("writer families");
    assert!(set.families.is_empty());
    assert!(
        set.unverifiable,
        "a fact with no episode source cannot vouch for its writer"
    );

    // A fact id that resolves to no live node.
    let set = store
        .writer_families_for_facts(&[Id::from_content_hash(b"never-written")])
        .expect("writer families");
    assert!(set.unverifiable, "an invisible source cannot vouch either");

    // An agent-less chain: episode with no origin block and an agent id that was
    // never enrolled resolves to nothing.
    let (fact_id, _) = seeded_fact(&store, "a chain that dead-ends");
    let episode = insert_episode(&store, Id::from_content_hash(b"ghost"), b"ep-ghost", None);
    derive_fact(&store, &fact_id, &episode);
    let set = store
        .writer_families_for_facts(&[fact_id])
        .expect("writer families");
    assert!(set.families.is_empty());
    assert!(
        set.unverifiable,
        "no record, no origin, no agent: unverifiable"
    );
}

#[test]
fn a_recorded_empty_family_is_unverifiable_not_a_fallthrough() {
    let store = store();
    let agent = enroll_agent(&store, "claude-opus-4-8", b"agent-c");
    let (fact_id, _) = seeded_fact(&store, "an empty record must not launder");
    let episode = insert_episode(
        &store,
        agent,
        b"ep-empty",
        Some(origin_with(Some("claude-opus-4-8"))),
    );
    // The signed record asserts an empty family: that emptiness is final.
    provenance_with_family(&store, &episode, "   ");
    derive_fact(&store, &fact_id, &episode);

    let set = store
        .writer_families_for_facts(&[fact_id])
        .expect("writer families");
    assert!(
        set.families.is_empty(),
        "a recorded-empty family must not fall through to origin or agent: {:?}",
        set.families
    );
    assert!(set.unverifiable, "recorded-empty reads as unverifiable");
}

#[test]
fn note_families_union_the_distilling_model() {
    let store = store();
    let agent = enroll_agent(&store, "unused", b"agent-d");
    let (fact_id, _) = seeded_fact(&store, "the note's sources");
    let episode = insert_episode(
        &store,
        agent,
        b"ep-note",
        Some(origin_with(Some("claude-sonnet-4-6"))),
    );
    derive_fact(&store, &fact_id, &episode);

    // A rule summary carries no Distill audit: only the episode writers count.
    let rule_note = insert_note(&store, b"rule-note", &[&fact_id]);
    let set = store
        .writer_families_for_note(&rule_note)
        .expect("note families");
    assert_eq!(set.families, vec!["claude-sonnet-4-6".to_string()]);
    assert!(!set.unverifiable);

    // A distilled note unions the model that authored its content — the two-hop
    // launder (distill with X, evolve with X) must see X among the writers.
    let distilled = insert_note(&store, b"distilled-note", &[&fact_id]);
    distill_audit(&store, &distilled, Some("gpt-5"), Some("2026-01"));
    let set = store
        .writer_families_for_note(&distilled)
        .expect("note families");
    assert_eq!(
        set.families,
        vec!["claude-sonnet-4-6".to_string(), "gpt-5".to_string()],
        "the distilling model joins the writer set"
    );
    assert!(!set.unverifiable);

    // A Distill event that recorded a null family is an inference author nobody
    // can vouch for.
    let laundered = insert_note(&store, b"laundered-note", &[&fact_id]);
    distill_audit(&store, &laundered, None, None);
    let set = store
        .writer_families_for_note(&laundered)
        .expect("note families");
    assert!(
        set.unverifiable,
        "a null distilling family is unverifiable, not absent"
    );

    // A note id that was never written.
    let set = store
        .writer_families_for_note(&Id::from_content_hash(b"no-such-note"))
        .expect("note families");
    assert!(set.unverifiable);
}

#[test]
fn distinct_agent_families_skip_blanks_and_sort() {
    let store = store();
    enroll_agent(&store, "claude-sonnet-4-6", b"agent-e1");
    enroll_agent(&store, "claude-sonnet-4-6", b"agent-e2");
    enroll_agent(&store, "gpt-5", b"agent-e3");
    enroll_agent(&store, "   ", b"agent-e4");

    assert_eq!(
        store.distinct_agent_families().expect("families"),
        vec!["claude-sonnet-4-6".to_string(), "gpt-5".to_string()],
        "distinct, sorted, blank declarations skipped"
    );
}

#[test]
fn note_lineage_bundles_sources_model_and_writers() {
    let store = store();
    let agent = enroll_agent(&store, "unused", b"agent-f");
    let (fact_a, _) = seeded_fact(&store, "first source");
    let (fact_b, _) = seeded_fact(&store, "second source");
    let ep_a = insert_episode(
        &store,
        agent,
        b"ep-lineage-a",
        Some(origin_with(Some("claude-sonnet-4-6"))),
    );
    let ep_b = insert_episode(
        &store,
        agent,
        b"ep-lineage-b",
        Some(origin_with(Some("mistral-large"))),
    );
    derive_fact(&store, &fact_a, &ep_a);
    derive_fact(&store, &fact_b, &ep_b);
    let note = insert_note(&store, b"lineage-note", &[&fact_a, &fact_b]);
    distill_audit(&store, &note, Some("gpt-5"), Some("2026-01"));

    let lineage = store
        .note_lineage(&note)
        .expect("note lineage")
        .expect("note exists");
    let mut expected_facts = vec![fact_a, fact_b];
    expected_facts.sort_unstable();
    let mut expected_episodes = vec![ep_a, ep_b];
    expected_episodes.sort_unstable();
    assert_eq!(lineage.note, note);
    assert_eq!(lineage.source_facts, expected_facts);
    assert_eq!(lineage.source_episodes, expected_episodes);
    let model = lineage.consolidating_model.expect("a distilled note");
    assert_eq!(model.family.as_deref(), Some("gpt-5"));
    assert_eq!(model.version.as_deref(), Some("2026-01"));
    assert_eq!(
        lineage.writer_families.families,
        vec![
            "claude-sonnet-4-6".to_string(),
            "gpt-5".to_string(),
            "mistral-large".to_string(),
        ]
    );
    assert!(!lineage.writer_families.unverifiable);
    assert!(
        lineage.non_canonical,
        "a note is structurally non-canonical"
    );

    assert_eq!(
        store
            .note_lineage(&Id::from_content_hash(b"unwritten"))
            .expect("read"),
        None,
        "no live note, no lineage"
    );
}
