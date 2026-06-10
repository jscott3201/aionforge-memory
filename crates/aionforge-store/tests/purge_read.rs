//! Store-level tests for the erasure-cascade closure walk (05 §3, M5.T03): the
//! fixed-point transitive closure over incoming `DERIVED_FROM`, the multi-parent
//! survival rule, the cycle guard, cap refusals decided read-only, shared entities
//! never followed, and the exclusively-owned provenance additions.

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind, ProvenanceRecord};
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::time::Timestamp;
use aionforge_domain::value::ObjectValue;
use aionforge_store::{
    BoundQuery, CascadeCaps, ClosureOutcome, NodeId, PurgeClosure, Store, StoreConfig, Value,
};

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

fn now() -> Timestamp {
    ts("2026-06-06T12:00:00-05:00[America/Chicago]")
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

fn caps() -> CascadeCaps {
    CascadeCaps {
        max_depth: 16,
        max_nodes: 200,
    }
}

fn identity() -> Identity {
    Identity {
        id: Id::generate(),
        ingested_at: ts("2026-06-01T09:00:00-05:00[America/Chicago]"),
        namespace: Namespace::Global,
        expired_at: None,
    }
}

fn stats() -> Stats {
    Stats {
        importance: 0.5,
        trust: 0.5,
        last_access: ts("2026-06-01T09:00:00-05:00[America/Chicago]"),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

fn episode(content: &str) -> Episode {
    Episode {
        identity: identity(),
        stats: stats(),
        content: content.to_string(),
        role: Role::User,
        captured_at: now(),
        agent_id: Id::from_content_hash(b"purge-agent"),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    }
}

fn fact(statement: &str) -> Fact {
    Fact {
        identity: identity(),
        stats: stats(),
        subject_id: Id::from_content_hash(b"subject"),
        predicate: "tests".to_string(),
        object: ObjectValue::Text(statement.to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: statement.to_string(),
        embedding: None,
        embedder_model: None,
        extraction: None,
        cooled_until: None,
    }
}

fn derived_edge(store: &Store, query: &'static str, from: &Id, to: &Id) {
    let bound = BoundQuery::new(query)
        .bind_uuid("from", from)
        .unwrap()
        .bind_uuid("to", to)
        .unwrap()
        .bind("ts", Value::ZonedDateTime(Box::new(now())))
        .unwrap();
    store.execute(&bound).expect("insert DERIVED_FROM edge");
}

const FACT_FROM_EPISODE: &str = "MATCH (a:Fact {id: $from}), (b:Episode {id: $to}) \
     INSERT (a)-[:DERIVED_FROM {derived_at: $ts}]->(b)";
const FACT_FROM_FACT: &str = "MATCH (a:Fact {id: $from}), (b:Fact {id: $to}) \
     INSERT (a)-[:DERIVED_FROM {derived_at: $ts}]->(b)";
const EPISODE_FROM_EPISODE: &str = "MATCH (a:Episode {id: $from}), (b:Episode {id: $to}) \
     INSERT (a)-[:DERIVED_FROM {derived_at: $ts}]->(b)";

fn mentions_edge(store: &Store, episode_id: &Id, entity_id: &Id) {
    let bound = BoundQuery::new(
        "MATCH (a:Episode {id: $from}), (b:Entity {id: $to}) \
         INSERT (a)-[:MENTIONS {valid_from: $ts, ingested_at: $ts}]->(b)",
    )
    .bind_uuid("from", episode_id)
    .unwrap()
    .bind_uuid("to", entity_id)
    .unwrap()
    .bind("ts", Value::ZonedDateTime(Box::new(now())))
    .unwrap();
    store.execute(&bound).expect("insert MENTIONS edge");
}

fn computed(outcome: ClosureOutcome) -> PurgeClosure {
    match outcome {
        ClosureOutcome::Computed(closure) => closure,
        other => panic!("expected a computed closure, got {other:?}"),
    }
}

#[test]
fn a_linear_chain_and_fanout_close_transitively() {
    let store = store();
    let e = episode("source episode");
    let f1 = fact("first derivative");
    let f2 = fact("second derivative");
    let f1b = fact("derivative of the first derivative");
    let e_node = store.insert_episode(&e).expect("insert");
    let f1_node = store.insert_fact(&f1).expect("insert");
    let f2_node = store.insert_fact(&f2).expect("insert");
    let f1b_node = store.insert_fact(&f1b).expect("insert");
    derived_edge(&store, FACT_FROM_EPISODE, &f1.identity.id, &e.identity.id);
    derived_edge(&store, FACT_FROM_EPISODE, &f2.identity.id, &e.identity.id);
    derived_edge(&store, FACT_FROM_FACT, &f1b.identity.id, &f1.identity.id);

    let closure = computed(store.derived_from_closure(e_node, &caps()).expect("walk"));
    let nodes: Vec<NodeId> = closure.nodes.clone();
    assert_eq!(nodes.len(), 4, "seed + two children + one grandchild");
    for member in [e_node, f1_node, f2_node, f1b_node] {
        assert!(nodes.contains(&member));
    }
    assert_eq!(closure.cascade_depth, 2);
    assert_eq!(closure.nodes[0], e_node, "the seed leads the closure");
    assert_eq!(closure.node_ids[0], e.identity.id, "ids are index-parallel");
    assert!(closure.spared_multiparent.is_empty());
    assert_eq!(closure.provenance_count, 0);
}

#[test]
fn a_multi_parent_derivative_survives_and_is_reported() {
    let store = store();
    let e1 = episode("erased source");
    let e2 = episode("surviving source");
    let shared = fact("deduped fact derived from both");
    let e1_node = store.insert_episode(&e1).expect("insert");
    let e2_node = store.insert_episode(&e2).expect("insert");
    let shared_node = store.insert_fact(&shared).expect("insert");
    derived_edge(
        &store,
        FACT_FROM_EPISODE,
        &shared.identity.id,
        &e1.identity.id,
    );
    derived_edge(
        &store,
        FACT_FROM_EPISODE,
        &shared.identity.id,
        &e2.identity.id,
    );

    let closure = computed(store.derived_from_closure(e1_node, &caps()).expect("walk"));
    assert_eq!(closure.nodes, vec![e1_node], "only the seed is doomed");
    assert!(!closure.nodes.contains(&shared_node));
    assert!(!closure.nodes.contains(&e2_node));
    assert_eq!(
        closure.spared_multiparent,
        vec![shared.identity.id],
        "the survivor is reported, never silently skipped"
    );
}

#[test]
fn the_fixed_point_admits_a_late_arriving_sibling_source() {
    let store = store();
    // E2 is itself derived from E1; F is derived from BOTH. When F is discovered via
    // E1 its source E2 may not be doomed yet — only the fixed-point re-evaluation
    // admits it. A single forward pass would wrongly spare F.
    let e1 = episode("root source");
    let e2 = episode("derived source");
    let f = fact("derived from both");
    let e1_node = store.insert_episode(&e1).expect("insert");
    let e2_node = store.insert_episode(&e2).expect("insert");
    let f_node = store.insert_fact(&f).expect("insert");
    derived_edge(
        &store,
        EPISODE_FROM_EPISODE,
        &e2.identity.id,
        &e1.identity.id,
    );
    derived_edge(&store, FACT_FROM_EPISODE, &f.identity.id, &e1.identity.id);
    derived_edge(&store, FACT_FROM_EPISODE, &f.identity.id, &e2.identity.id);

    let closure = computed(store.derived_from_closure(e1_node, &caps()).expect("walk"));
    assert_eq!(closure.nodes.len(), 3, "all three fall together");
    for member in [e1_node, e2_node, f_node] {
        assert!(closure.nodes.contains(&member));
    }
    assert!(closure.spared_multiparent.is_empty());
    assert_eq!(
        closure.cascade_depth, 2,
        "F sits one past its deepest source"
    );
}

#[test]
fn a_malformed_cycle_terminates() {
    let store = store();
    let a = fact("cycle a");
    let b = fact("cycle b");
    let a_node = store.insert_fact(&a).expect("insert");
    let b_node = store.insert_fact(&b).expect("insert");
    derived_edge(&store, FACT_FROM_FACT, &a.identity.id, &b.identity.id);
    derived_edge(&store, FACT_FROM_FACT, &b.identity.id, &a.identity.id);

    let closure = computed(store.derived_from_closure(a_node, &caps()).expect("walk"));
    assert_eq!(
        closure.nodes.len(),
        2,
        "both cycle members, exactly once each"
    );
    assert!(closure.nodes.contains(&a_node));
    assert!(closure.nodes.contains(&b_node));
}

#[test]
fn exceeding_either_cap_refuses_before_any_write_could_follow() {
    let store = store();
    let e = episode("capped source");
    let f1 = fact("level one");
    let f2 = fact("level two");
    let e_node = store.insert_episode(&e).expect("insert");
    store.insert_fact(&f1).expect("insert");
    store.insert_fact(&f2).expect("insert");
    derived_edge(&store, FACT_FROM_EPISODE, &f1.identity.id, &e.identity.id);
    derived_edge(&store, FACT_FROM_FACT, &f2.identity.id, &f1.identity.id);

    let depth_refused = store
        .derived_from_closure(
            e_node,
            &CascadeCaps {
                max_depth: 1,
                max_nodes: 200,
            },
        )
        .expect("walk");
    assert!(
        matches!(depth_refused, ClosureOutcome::TooLarge { depth_observed, .. } if depth_observed == 2),
        "the depth cap refuses: {depth_refused:?}"
    );

    let node_refused = store
        .derived_from_closure(
            e_node,
            &CascadeCaps {
                max_depth: 16,
                max_nodes: 2,
            },
        )
        .expect("walk");
    assert!(
        matches!(node_refused, ClosureOutcome::TooLarge { nodes_observed, .. } if nodes_observed == 3),
        "the node cap refuses: {node_refused:?}"
    );
}

#[test]
fn shared_entities_are_never_followed() {
    let store = store();
    let e = episode("entity-mentioning source");
    let entity = Entity {
        identity: identity(),
        stats: stats(),
        canonical_name: "selene".to_string(),
        entity_type: "Project".to_string(),
        aliases: Vec::new(),
        description: None,
        embedding: None,
        embedder_model: None,
        attributes: None,
    };
    let e_node = store.insert_episode(&e).expect("insert");
    let entity_node = store.insert_entity(&entity).expect("insert");
    mentions_edge(&store, &e.identity.id, &entity.identity.id);

    let closure = computed(store.derived_from_closure(e_node, &caps()).expect("walk"));
    assert_eq!(closure.nodes, vec![e_node]);
    assert!(
        !closure.nodes.contains(&entity_node),
        "MENTIONS is not a cascade edge; the shared entity survives"
    );
}

#[test]
fn a_captured_episode_brings_its_provenance_record() {
    let store = store();
    let e = episode("captured with provenance");
    let provenance = ProvenanceRecord {
        identity: identity(),
        subject_id: e.identity.id,
        writer_agent_id: Id::from_content_hash(b"purge-agent"),
        signature: "sig".to_string(),
        source_episode_ids: Vec::new(),
        model_family: "test".to_string(),
        model_version: None,
        trust_at_write: 0.5,
    };
    let audit = AuditEvent {
        identity: identity(),
        kind: AuditKind::Capture,
        subject_id: e.identity.id,
        actor_id: Id::from_content_hash(b"purge-agent"),
        payload: serde_json::json!({"reason": "test"}),
        signature: String::new(),
        occurred_at: now(),
    };
    let ids = store
        .commit_capture(&e, &provenance, &audit)
        .expect("capture");

    let closure = computed(
        store
            .derived_from_closure(ids.episode, &caps())
            .expect("walk"),
    );
    assert_eq!(
        closure.nodes.len(),
        2,
        "the episode and its provenance record"
    );
    assert!(closure.nodes.contains(&ids.provenance));
    assert_eq!(closure.provenance_count, 1);
    assert!(
        closure.node_ids.contains(&provenance.identity.id),
        "the provenance record resolves into the id-only spine"
    );
}

#[test]
fn a_fact_seed_with_no_derivatives_is_a_singleton_closure() {
    let store = store();
    let f = fact("standalone");
    let f_node = store.insert_fact(&f).expect("insert");
    let closure = computed(store.derived_from_closure(f_node, &caps()).expect("walk"));
    assert_eq!(closure.nodes, vec![f_node]);
    assert_eq!(closure.cascade_depth, 0);
}

#[test]
fn a_cascaded_node_brings_its_own_provenance_and_trails_the_order() {
    let store = store();
    // Two captured episodes, each with its own provenance record; the second derived
    // from the first. Purging the first cascades the second AND both records.
    let e1 = episode("captured root");
    let e2 = episode("captured derivative");
    let capture = |ep: &Episode, tag: &[u8]| {
        let provenance = ProvenanceRecord {
            identity: identity(),
            subject_id: ep.identity.id,
            writer_agent_id: Id::from_content_hash(b"purge-agent"),
            signature: "sig".to_string(),
            source_episode_ids: Vec::new(),
            model_family: "test".to_string(),
            model_version: None,
            trust_at_write: 0.5,
        };
        let audit = AuditEvent {
            identity: Identity {
                id: Id::from_content_hash(tag),
                ingested_at: now(),
                namespace: Namespace::Global,
                expired_at: None,
            },
            kind: AuditKind::Capture,
            subject_id: ep.identity.id,
            actor_id: Id::from_content_hash(b"purge-agent"),
            payload: serde_json::json!({"reason": "test"}),
            signature: String::new(),
            occurred_at: now(),
        };
        let ids = store
            .commit_capture(ep, &provenance, &audit)
            .expect("capture");
        (ids, provenance.identity.id)
    };
    let (e1_ids, p1_id) = capture(&e1, b"cap-1");
    let (e2_ids, p2_id) = capture(&e2, b"cap-2");
    derived_edge(
        &store,
        EPISODE_FROM_EPISODE,
        &e2.identity.id,
        &e1.identity.id,
    );

    let closure = computed(
        store
            .derived_from_closure(e1_ids.episode, &caps())
            .expect("walk"),
    );
    assert_eq!(
        closure.nodes.len(),
        4,
        "two episodes, two provenance records"
    );
    assert_eq!(closure.provenance_count, 2);
    for member in [e1_ids.provenance, e2_ids.provenance] {
        assert!(closure.nodes.contains(&member));
    }
    // The provenance block trails the admission order.
    assert_eq!(closure.node_ids[..2], [e1.identity.id, e2.identity.id]);
    assert!(closure.node_ids[2..].contains(&p1_id));
    assert!(closure.node_ids[2..].contains(&p2_id));

    // The same shape against a node cap the provenance pass overruns: the walk admits
    // both episodes, then refuses when the records would push past the cap.
    let refused = store
        .derived_from_closure(
            e1_ids.episode,
            &CascadeCaps {
                max_depth: 16,
                max_nodes: 3,
            },
        )
        .expect("walk");
    assert!(
        matches!(
            refused,
            ClosureOutcome::TooLarge {
                nodes_observed: 4,
                ..
            }
        ),
        "the node cap holds through the provenance pass: {refused:?}"
    );
}

#[test]
fn a_soft_forgotten_seed_still_closes() {
    let store = store();
    let f = fact("forgotten then erased");
    let f_node = store.insert_fact(&f).expect("insert");
    let forget_audit = AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(b"forget-before-erase"),
            ingested_at: now(),
            namespace: Namespace::Global,
            expired_at: None,
        },
        kind: AuditKind::Forget,
        subject_id: f.identity.id,
        actor_id: Id::from_content_hash(b"purge-agent"),
        payload: serde_json::json!({"reason": "test"}),
        signature: String::new(),
        occurred_at: now(),
    };
    store
        .soft_forget(f_node, &now(), &forget_audit)
        .expect("soft forget");

    let closure = computed(store.derived_from_closure(f_node, &caps()).expect("walk"));
    assert_eq!(
        closure.nodes,
        vec![f_node],
        "the walk reads no expiry: erasure escalates past a soft-forget"
    );
}

#[test]
fn a_closure_exactly_at_the_depth_cap_is_allowed() {
    let store = store();
    let e = episode("depth-bounded source");
    let f1 = fact("level one");
    let f2 = fact("level two");
    let e_node = store.insert_episode(&e).expect("insert");
    store.insert_fact(&f1).expect("insert");
    store.insert_fact(&f2).expect("insert");
    derived_edge(&store, FACT_FROM_EPISODE, &f1.identity.id, &e.identity.id);
    derived_edge(&store, FACT_FROM_FACT, &f2.identity.id, &f1.identity.id);

    let closure = computed(
        store
            .derived_from_closure(
                e_node,
                &CascadeCaps {
                    max_depth: 2,
                    max_nodes: 200,
                },
            )
            .expect("walk"),
    );
    assert_eq!(closure.cascade_depth, 2, "exactly at the cap is within it");
    assert_eq!(closure.nodes.len(), 3);
}

#[test]
fn multiple_spared_survivors_are_all_reported() {
    let store = store();
    let erased = episode("erased source");
    let surviving = episode("surviving source");
    let shared_a = fact("first shared derivative");
    let shared_b = fact("second shared derivative");
    let erased_node = store.insert_episode(&erased).expect("insert");
    store.insert_episode(&surviving).expect("insert");
    store.insert_fact(&shared_a).expect("insert");
    store.insert_fact(&shared_b).expect("insert");
    for shared in [&shared_a, &shared_b] {
        derived_edge(
            &store,
            FACT_FROM_EPISODE,
            &shared.identity.id,
            &erased.identity.id,
        );
        derived_edge(
            &store,
            FACT_FROM_EPISODE,
            &shared.identity.id,
            &surviving.identity.id,
        );
    }

    let closure = computed(
        store
            .derived_from_closure(erased_node, &caps())
            .expect("walk"),
    );
    assert_eq!(closure.nodes, vec![erased_node]);
    assert_eq!(
        closure.spared_multiparent.len(),
        2,
        "both survivors reported"
    );
    for shared in [&shared_a, &shared_b] {
        assert!(closure.spared_multiparent.contains(&shared.identity.id));
    }
}

#[test]
fn a_dead_seed_is_the_typed_not_live_outcome() {
    let store = store();
    let f = fact("purged before the walk");
    let f_node = store.insert_fact(&f).expect("insert");
    let purge_audit = AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(b"pre-walk-purge"),
            ingested_at: now(),
            namespace: Namespace::Global,
            expired_at: None,
        },
        kind: AuditKind::Purge,
        subject_id: f.identity.id,
        actor_id: Id::from_content_hash(b"purge-agent"),
        payload: serde_json::json!({"reason": "test"}),
        signature: String::new(),
        occurred_at: now(),
    };
    store.hard_purge(&[f_node], &purge_audit).expect("purge");

    let outcome = store.derived_from_closure(f_node, &caps()).expect("walk");
    assert_eq!(
        outcome,
        ClosureOutcome::SeedNotLive,
        "a dead seed is a typed outcome, not an error"
    );
}

#[test]
fn a_pending_chain_unlocks_transitively() {
    let store = store();
    // C derives only from the seed; A derives from the seed AND C; B derives from the
    // seed AND A. Whatever order discovery meets them, A can only doom after C, and B
    // only after A — the sweep must keep re-running while it makes progress.
    let s = episode("the seed");
    let c = fact("unlocks first");
    let a = fact("unlocks second");
    let b = fact("unlocks third");
    let s_node = store.insert_episode(&s).expect("insert");
    let c_node = store.insert_fact(&c).expect("insert");
    let a_node = store.insert_fact(&a).expect("insert");
    let b_node = store.insert_fact(&b).expect("insert");
    derived_edge(&store, FACT_FROM_EPISODE, &c.identity.id, &s.identity.id);
    derived_edge(&store, FACT_FROM_EPISODE, &a.identity.id, &s.identity.id);
    derived_edge(&store, FACT_FROM_FACT, &a.identity.id, &c.identity.id);
    derived_edge(&store, FACT_FROM_EPISODE, &b.identity.id, &s.identity.id);
    derived_edge(&store, FACT_FROM_FACT, &b.identity.id, &a.identity.id);

    let closure = computed(store.derived_from_closure(s_node, &caps()).expect("walk"));
    assert_eq!(closure.nodes.len(), 4, "the whole chain unlocks");
    for member in [s_node, c_node, a_node, b_node] {
        assert!(closure.nodes.contains(&member));
    }
    assert!(closure.spared_multiparent.is_empty());
    assert_eq!(
        closure.cascade_depth, 3,
        "B sits one past A past C past the seed"
    );
}

#[test]
fn the_closure_reports_its_namespace_span_deduplicated_in_encounter_order() {
    let store = store();
    // Seed in an agent namespace; one derivative beside it, one across a team
    // boundary. The span is what the erase orchestrator authorizes against, so it
    // must cover every member exactly once, the seed's namespace first.
    let agent_ns = Namespace::Agent("span-owner".to_string());
    let team_ns = Namespace::Team("atlas".to_string());
    let mut e = episode("the spanning source");
    e.identity.namespace = agent_ns.clone();
    let mut sibling = fact("derivative beside the seed");
    sibling.identity.namespace = agent_ns.clone();
    let mut crossed = fact("derivative across the boundary");
    crossed.identity.namespace = team_ns.clone();
    let seed = store.insert_episode(&e).expect("insert");
    store.insert_fact(&sibling).expect("insert");
    store.insert_fact(&crossed).expect("insert");
    derived_edge(
        &store,
        FACT_FROM_EPISODE,
        &sibling.identity.id,
        &e.identity.id,
    );
    derived_edge(
        &store,
        FACT_FROM_EPISODE,
        &crossed.identity.id,
        &e.identity.id,
    );

    let closure = computed(store.derived_from_closure(seed, &caps()).expect("walk"));
    assert_eq!(closure.nodes.len(), 3);
    assert_eq!(
        closure.namespaces.len(),
        2,
        "two members share a namespace: the span deduplicates"
    );
    assert_eq!(
        closure.namespaces[0], agent_ns,
        "the seed's own namespace leads the span"
    );
    assert!(closure.namespaces.contains(&team_ns));
}
