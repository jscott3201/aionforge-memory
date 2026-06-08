//! Integration tests for the capture write primitives (04 §1): the single-funnel
//! capture commit, its edges, and the content-hash dedup probe.

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind, ProvenanceRecord};
use aionforge_domain::time::Timestamp;

use aionforge_store::{BoundQuery, QueryResult, Store, StoreConfig, Value};

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

fn store() -> Store {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store
        .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
        .expect("migrate store");
    store
}

fn episode(content: &str) -> Episode {
    Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts("2026-06-06T09:30:00-05:00[America/Chicago]"),
            namespace: Namespace::Agent("alice".to_string()),
            expired_at: None,
        },
        stats: Stats {
            importance: 0.5,
            trust: 0.8,
            last_access: ts("2026-06-06T10:00:00-05:00[America/Chicago]"),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.1,
            is_pinned: false,
        },
        content: content.to_string(),
        role: Role::User,
        captured_at: ts("2026-06-06T09:29:59-05:00[America/Chicago]"),
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    }
}

fn provenance(subject: &Id, writer: &Id) -> ProvenanceRecord {
    ProvenanceRecord {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts("2026-06-06T09:30:00-05:00[America/Chicago]"),
            namespace: Namespace::Agent("alice".to_string()),
            expired_at: None,
        },
        subject_id: subject.clone(),
        writer_agent_id: writer.clone(),
        signature: String::new(),
        source_episode_ids: Vec::new(),
        model_family: "test-embedder".to_string(),
        model_version: Some("1".to_string()),
        trust_at_write: 0.8,
    }
}

fn audit(subject: &Id, actor: &Id) -> AuditEvent {
    AuditEvent {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts("2026-06-06T09:30:00-05:00[America/Chicago]"),
            namespace: Namespace::Agent("alice".to_string()),
            expired_at: None,
        },
        kind: AuditKind::Capture,
        subject_id: subject.clone(),
        actor_id: actor.clone(),
        payload: serde_json::json!({ "dedup": "new" }),
        signature: String::new(),
        occurred_at: ts("2026-06-06T09:30:00-05:00[America/Chicago]"),
    }
}

#[test]
fn commit_capture_writes_the_bundle_and_round_trips() {
    let store = store();
    let ep = episode("the user asked about retrieval");
    let pv = provenance(&ep.identity.id, &ep.agent_id);
    let au = audit(&ep.identity.id, &ep.agent_id);

    let ids = store.commit_capture(&ep, &pv, &au).expect("commit capture");

    let read_ep = store
        .episode_by_node_id(ids.episode)
        .expect("read episode")
        .expect("episode present");
    assert_eq!(read_ep, ep);
    let read_pv = store
        .provenance_by_node_id(ids.provenance)
        .expect("read provenance")
        .expect("provenance present");
    assert_eq!(read_pv, pv);
    let read_au = store
        .audit_event_by_node_id(ids.audit)
        .expect("read audit")
        .expect("audit present");
    assert_eq!(read_au, au);
}

#[test]
fn commit_capture_wires_the_edges() {
    let store = store();
    let ep = episode("an event worth proving");
    let pv = provenance(&ep.identity.id, &ep.agent_id);
    let au = audit(&ep.identity.id, &ep.agent_id);
    store.commit_capture(&ep, &pv, &au).expect("commit capture");

    // Episode -HAS_PROVENANCE-> ProvenanceRecord
    let prov = store
        .execute(
            &BoundQuery::new(
                "MATCH (e:Episode)-[:HAS_PROVENANCE]->(p:ProvenanceRecord) \
                 WHERE e.id = $id RETURN p.id AS id",
            )
            .bind_str("id", ep.identity.id.as_str())
            .expect("bind"),
        )
        .expect("provenance edge query");
    assert_eq!(first_id(&prov).as_deref(), Some(pv.identity.id.as_str()));

    // AuditEvent -AUDIT-> Episode
    let audit_rows = store
        .execute(
            &BoundQuery::new(
                "MATCH (a:AuditEvent)-[:AUDIT]->(e:Episode) \
                 WHERE e.id = $id RETURN a.id AS id",
            )
            .bind_str("id", ep.identity.id.as_str())
            .expect("bind"),
        )
        .expect("audit edge query");
    assert_eq!(
        first_id(&audit_rows).as_deref(),
        Some(au.identity.id.as_str())
    );
}

#[test]
fn content_hash_probe_finds_committed_and_misses_unknown() {
    let store = store();
    let ep = episode("a captured turn");
    let pv = provenance(&ep.identity.id, &ep.agent_id);
    let au = audit(&ep.identity.id, &ep.agent_id);
    store.commit_capture(&ep, &pv, &au).expect("commit capture");

    let found = store
        .episode_id_by_content_hash(&ep.content_hash)
        .expect("probe");
    assert_eq!(found, Some(ep.identity.id));

    let missing = store
        .episode_id_by_content_hash(&ContentHash::of(b"never captured"))
        .expect("probe");
    assert_eq!(missing, None);
}

fn first_id(result: &QueryResult) -> Option<String> {
    match result {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::String(s)) => Some(s.as_str().to_string()),
            _ => None,
        },
        _ => None,
    }
}

#[test]
fn commit_audit_writes_a_standalone_event_that_round_trips() {
    let store = store();
    let agent = Id::generate();
    // A `namespace_denied` rejection audit: the agent is both subject and actor, the event lives in
    // the system namespace, and the payload carries the requested namespace and the deny reason.
    let mut event = audit(&agent, &agent);
    event.kind = AuditKind::NamespaceDenied;
    event.identity.namespace = Namespace::System;
    event.payload = serde_json::json!({
        "requested_namespace": "team:squad",
        "reason": "not a member of the team",
        "agent": agent.as_str(),
    });

    let node_id = store.commit_audit(&event).expect("commit_audit");

    // Read every field back through the node id `commit_audit` returns (no episode, no edge).
    let read = store
        .audit_event_by_node_id(node_id)
        .expect("read")
        .expect("the audit node exists");
    assert_eq!(read.kind, AuditKind::NamespaceDenied);
    assert_eq!(read.subject_id, agent, "subject is the acting agent");
    assert_eq!(read.actor_id, agent);
    assert_eq!(read.identity.namespace, Namespace::System);
    assert_eq!(read.payload["requested_namespace"], "team:squad");
    assert_eq!(read.payload["reason"], "not a member of the team");
    assert_eq!(read.payload["agent"], agent.as_str());

    // It is discoverable by the scalar `kind` index, and no Episode or AUDIT edge was written.
    assert_eq!(
        count(
            &store,
            "MATCH (a:AuditEvent) WHERE a.kind = 'namespace_denied' RETURN count(a) AS n",
        ),
        1,
    );
    assert_eq!(
        count(&store, "MATCH (e:Episode) RETURN count(e) AS n"),
        0,
        "a rejection writes no memory",
    );
    assert_eq!(
        count(
            &store,
            "MATCH (:AuditEvent)-[r:AUDIT]->() RETURN count(r) AS n",
        ),
        0,
        "no AUDIT edge is wired for a subject without a node",
    );
}

fn count(store: &Store, pattern: &str) -> u64 {
    match store.execute(&BoundQuery::new(pattern)).expect("count") {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Uint(n)) => *n,
            Some(Value::Int(n)) => u64::try_from(*n).unwrap_or(0),
            _ => 0,
        },
        _ => 0,
    }
}
