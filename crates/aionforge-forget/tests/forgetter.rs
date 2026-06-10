//! Orchestrator acceptance for active forgetting (05 §2, M5.T02): every protection
//! spares, only all-axes-low Episode/Fact candidates are swept, point ops report which
//! protection held, and the audit trail records the decision basis in the memory's own
//! namespace.

use std::collections::BTreeMap;
use std::sync::Arc;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::edges::AttestedBy;
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::agent::{Agent, AgentStatus, TrustScores};
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::procedural::{BadPattern, Skill};
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::time::Timestamp;
use aionforge_domain::value::ObjectValue;
use aionforge_forget::{Forgetter, ForgettingPolicy, PointForget, PointUnforget, SpareReason};
use aionforge_store::{BoundQuery, Store, StoreConfig, Value};

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

fn now() -> Timestamp {
    ts("2026-06-06T12:00:00-05:00[America/Chicago]")
}

/// Five months before `now`: past any sane min-age.
fn long_ago() -> Timestamp {
    ts("2026-01-05T09:00:00-06:00[America/Chicago]")
}

fn store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store
        .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
        .expect("migrate store");
    Arc::new(store)
}

fn forgetter(store: &Arc<Store>) -> Forgetter {
    Forgetter::new(
        Arc::clone(store),
        ForgettingPolicy {
            enabled: true,
            ..ForgettingPolicy::default()
        },
    )
}

/// All-axes-low stats: stored importance below the floor, low trust, stale access.
fn low_stats() -> Stats {
    Stats {
        importance: 0.04,
        trust: 0.2,
        last_access: long_ago(),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

fn identity_in(namespace: Namespace) -> Identity {
    Identity {
        id: Id::generate(),
        ingested_at: long_ago(),
        namespace,
        expired_at: None,
    }
}

fn fact_with(stats: Stats, status: FactStatus) -> Fact {
    Fact {
        identity: identity_in(Namespace::Global),
        stats,
        subject_id: Id::from_content_hash(b"subject"),
        predicate: "tests".to_string(),
        object: ObjectValue::Text("forgetting".to_string()),
        confidence: 0.9,
        status,
        statement: "tests forgetting".to_string(),
        embedding: None,
        embedder_model: None,
        extraction: None,
    }
}

fn low_fact() -> Fact {
    fact_with(low_stats(), FactStatus::Active)
}

fn low_episode() -> Episode {
    let content = format!("episode {}", Id::generate());
    Episode {
        identity: identity_in(Namespace::Agent("tester".to_string())),
        stats: low_stats(),
        content: content.clone(),
        role: Role::User,
        captured_at: long_ago(),
        agent_id: Id::from_content_hash(b"test-agent"),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    }
}

fn support_edge(store: &Store, from: &Id, to: &Id) {
    let query = BoundQuery::new(
        "MATCH (a:Fact {id: $from}), (b:Fact {id: $to}) \
         INSERT (a)-[:SUPPORTS {weight: $weight}]->(b)",
    )
    .bind_uuid("from", from)
    .unwrap()
    .bind_uuid("to", to)
    .unwrap()
    .bind("weight", Value::Float(1.0))
    .unwrap();
    store.execute(&query).expect("insert SUPPORTS edge");
}

fn lineage_edge(store: &Store, from: &Id, to: &Id) {
    let query = BoundQuery::new(
        "MATCH (a:Fact {id: $from}), (b:Fact {id: $to}) \
         INSERT (a)-[:PROMOTED_TO {valid_from: $ts, ingested_at: $ts}]->(b)",
    )
    .bind_uuid("from", from)
    .unwrap()
    .bind_uuid("to", to)
    .unwrap()
    .bind("ts", Value::ZonedDateTime(Box::new(now())))
    .unwrap();
    store.execute(&query).expect("insert PROMOTED_TO edge");
}

/// Attest a fact so the attested exemption has a real `ATTESTED_BY` edge.
fn attest(store: &Store, fact_node: aionforge_store::NodeId) {
    let attester_id = Id::generate();
    let agent = Agent {
        identity: Identity {
            id: attester_id,
            ingested_at: now(),
            namespace: Namespace::Agent("ada".to_string()),
            expired_at: None,
        },
        public_key: "cHVibGljLWtleQ==".to_string(),
        model_family: "test".to_string(),
        model_version: None,
        trust_scores: TrustScores::default(),
        status: AgentStatus::Active,
    };
    let agent_node = store.create_agent(&agent).expect("enroll agent");
    let audit = AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(b"attest-audit"),
            ingested_at: now(),
            namespace: Namespace::Agent("ada".to_string()),
            expired_at: None,
        },
        kind: AuditKind::Attest,
        subject_id: attester_id,
        actor_id: attester_id,
        payload: serde_json::json!({"outcome": "accepted"}),
        signature: String::new(),
        occurred_at: now(),
    };
    store
        .attest_fact(
            fact_node,
            agent_node,
            &AttestedBy {
                attested_at: now(),
                signature: "sig".to_string(),
                category: None,
            },
            &audit,
        )
        .expect("attest");
}

fn is_expired(store: &Store, id: &Id) -> bool {
    store
        .memory_by_id(id, &["Episode", "Fact", "Entity", "Skill", "BadPattern"])
        .expect("resolve")
        .map(|c| c.identity.expired_at.is_some())
        .unwrap_or(false)
}

#[test]
fn the_sweep_forgets_only_all_axes_low_and_spares_every_protection() {
    let store = store();
    let forgetter = forgetter(&store);

    let eligible_fact = low_fact();
    let eligible_episode = low_episode();
    let pinned = fact_with(
        Stats {
            is_pinned: true,
            ..low_stats()
        },
        FactStatus::Active,
    );
    let trusted = fact_with(
        Stats {
            trust: 0.8,
            ..low_stats()
        },
        FactStatus::Active,
    );
    let important = fact_with(
        Stats {
            importance: 0.9,
            last_access: now(),
            ..low_stats()
        },
        FactStatus::Active,
    );
    let young = Fact {
        identity: Identity {
            ingested_at: now(),
            ..identity_in(Namespace::Global)
        },
        ..low_fact()
    };
    let supported = low_fact();
    let supporter = low_fact();
    let attested = low_fact();
    let lineage_a = low_fact();
    let lineage_b = low_fact();
    let contradicted = fact_with(low_stats(), FactStatus::Quarantined);

    for f in [
        &eligible_fact,
        &pinned,
        &trusted,
        &important,
        &young,
        &supported,
        &supporter,
        &lineage_a,
        &lineage_b,
        &contradicted,
    ] {
        store.insert_fact(f).expect("insert");
    }
    let attested_node = store.insert_fact(&attested).expect("insert");
    store.insert_episode(&eligible_episode).expect("insert");
    support_edge(&store, &supporter.identity.id, &supported.identity.id);
    lineage_edge(&store, &lineage_a.identity.id, &lineage_b.identity.id);
    attest(&store, attested_node);

    // Walk the whole population in one page.
    let report = forgetter.sweep_page(None, 200, &now()).expect("sweep");
    assert_eq!(report.scanned, 12, "every unexpired Episode/Fact evaluated");
    assert_eq!(
        report.forgotten, 3,
        "exactly the all-axes-low, unprotected candidates"
    );
    assert_eq!(
        report.spared, 9,
        "every protection tallied — the quarantined fact through the store's status refusal"
    );
    assert!(report.next.is_none());

    for (name, id, expect_forgotten) in [
        ("eligible fact", &eligible_fact.identity.id, true),
        ("eligible episode", &eligible_episode.identity.id, true),
        ("supporter (outgoing only)", &supporter.identity.id, true),
        ("pinned", &pinned.identity.id, false),
        ("high trust", &trusted.identity.id, false),
        ("high importance", &important.identity.id, false),
        ("young", &young.identity.id, false),
        ("referenced", &supported.identity.id, false),
        ("attested", &attested.identity.id, false),
        ("lineage source", &lineage_a.identity.id, false),
        ("lineage target", &lineage_b.identity.id, false),
        (
            "contradiction-quarantined",
            &contradicted.identity.id,
            false,
        ),
    ] {
        assert_eq!(is_expired(&store, id), expect_forgotten, "{name}");
    }

    // The audit trail: one Forget row per applied forget, in the memory's own
    // namespace, payload carrying the decision basis.
    let history = store
        .audit_by_kind(AuditKind::Forget, None, 50)
        .expect("audit");
    assert_eq!(history.events.len(), 3);
    let by_subject: BTreeMap<_, _> = history.events.iter().map(|e| (e.subject_id, e)).collect();
    let episode_row = by_subject
        .get(&eligible_episode.identity.id)
        .expect("episode forget audited");
    assert_eq!(
        episode_row.identity.namespace,
        Namespace::Agent("tester".to_string()),
        "audited in the memory's own namespace"
    );
    assert_eq!(episode_row.payload["reason"], "active_forgetting_sweep");
    assert_eq!(episode_row.payload["tier"], "episodic");
    assert!(episode_row.payload["decayed_importance"].is_number());

    // A second sweep over the same population converges: nothing left to forget,
    // no new audit rows.
    let again = forgetter.sweep_page(None, 200, &now()).expect("re-sweep");
    assert_eq!(again.forgotten, 0);
    assert_eq!(
        store
            .audit_by_kind(AuditKind::Forget, None, 50)
            .expect("audit")
            .events
            .len(),
        3
    );

    // The status refusal is a named outcome, not a silent skip: a point op on the
    // quarantined fact reports StatusOwned — another revision channel owns that node —
    // and leaves no audit row, because nothing flipped.
    assert_eq!(
        forgetter
            .forget(&contradicted.identity.id, &now())
            .expect("call"),
        PointForget::Protected(SpareReason::StatusOwned)
    );
    assert_eq!(
        store
            .audit_by_kind(AuditKind::Forget, None, 50)
            .expect("audit")
            .events
            .len(),
        3,
        "a refused forget audits nothing"
    );
}

#[test]
fn paged_sweep_equals_one_full_pass() {
    let store = store();
    let forgetter = Forgetter::new(
        Arc::clone(&store),
        ForgettingPolicy {
            enabled: true,
            batch_cap: 2,
            ..ForgettingPolicy::default()
        },
    );
    let facts: Vec<Fact> = (0..5).map(|_| low_fact()).collect();
    for f in &facts {
        store.insert_fact(f).expect("insert");
    }

    let mut cursor = None;
    let mut forgotten = 0;
    loop {
        let page = forgetter
            .sweep_page(cursor.as_ref(), 200, &now())
            .expect("page");
        forgotten += page.forgotten;
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(forgotten, 5, "the paged walk forgets the whole population");
    for f in &facts {
        assert!(is_expired(&store, &f.identity.id));
    }
}

#[test]
fn point_ops_gate_report_and_round_trip() {
    let store = store();
    let forgetter = forgetter(&store);

    // Unknown id.
    assert_eq!(
        forgetter.forget(&Id::generate(), &now()).expect("forget"),
        PointForget::NotFound
    );

    // A protected kind: entities are deferred from forgetting entirely.
    let entity = Entity {
        identity: identity_in(Namespace::Global),
        stats: low_stats(),
        canonical_name: "selene".to_string(),
        entity_type: "Project".to_string(),
        aliases: Vec::new(),
        description: None,
        embedding: None,
        embedder_model: None,
        attributes: None,
    };
    store.insert_entity(&entity).expect("insert entity");
    assert_eq!(
        forgetter.forget(&entity.identity.id, &now()).expect("call"),
        PointForget::Protected(SpareReason::ProtectedKind)
    );

    // A pinned memory reports the pin, not a silent no-op.
    let pinned = fact_with(
        Stats {
            is_pinned: true,
            ..low_stats()
        },
        FactStatus::Active,
    );
    store.insert_fact(&pinned).expect("insert");
    assert_eq!(
        forgetter.forget(&pinned.identity.id, &now()).expect("call"),
        PointForget::Protected(SpareReason::Pinned)
    );

    // The full cycle on an eligible fact.
    let fact = low_fact();
    store.insert_fact(&fact).expect("insert");
    assert_eq!(
        forgetter.forget(&fact.identity.id, &now()).expect("call"),
        PointForget::Forgotten
    );
    assert_eq!(
        forgetter.forget(&fact.identity.id, &now()).expect("call"),
        PointForget::AlreadyForgotten
    );
    assert_eq!(
        forgetter.unforget(&fact.identity.id, &now()).expect("call"),
        PointUnforget::Restored
    );
    assert_eq!(
        forgetter.unforget(&fact.identity.id, &now()).expect("call"),
        PointUnforget::NotForgotten
    );
    assert!(!is_expired(&store, &fact.identity.id));

    // Manual point-forgets record the manual reason.
    let row = &store
        .audit_by_kind(AuditKind::Forget, None, 10)
        .expect("audit")
        .events[0];
    assert_eq!(row.payload["reason"], "manual");
}

#[test]
fn the_bad_pattern_toggle_admits_the_kind_but_bypasses_no_axis() {
    let store = store();

    let body = "retry the deploy".to_string();
    let skill = Skill {
        identity: identity_in(Namespace::Global),
        stats: low_stats(),
        name: "deploy".to_string(),
        version: 1,
        description: "deploys the service".to_string(),
        problem_embedding: None,
        embedder_model: None,
        language: "python".to_string(),
        body: body.clone(),
        params: serde_json::json!({}),
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
        induced: false,
    };
    let skill_node = store.save_skill(&skill, None, &[]).expect("save skill");
    let pattern = BadPattern {
        identity: identity_in(Namespace::Global),
        stats: low_stats(),
        description: "rolled back on a bad config".to_string(),
        embedding: None,
        embedder_model: None,
        observed_at: long_ago(),
    };
    store
        .save_bad_pattern(&pattern, skill_node)
        .expect("save bad pattern");

    // Default policy: the kind is protected outright — the toggle is off.
    let off = forgetter(&store);
    assert_eq!(
        off.forget(&pattern.identity.id, &now()).expect("call"),
        PointForget::Protected(SpareReason::ProtectedKind)
    );

    // Toggle on: the kind gate opens and the normal axes take over. A normally-saved
    // pattern always carries its skill's `HAS_FAILURE` link, which is itself a
    // protecting reference — so the toggle admits the kind, it never bypasses an axis.
    let on = Forgetter::new(
        Arc::clone(&store),
        ForgettingPolicy {
            enabled: true,
            forget_bad_patterns: true,
            ..ForgettingPolicy::default()
        },
    );
    assert_eq!(
        on.forget(&pattern.identity.id, &now()).expect("call"),
        PointForget::Protected(SpareReason::Referenced)
    );
    assert!(!is_expired(&store, &pattern.identity.id));
}

#[test]
fn a_same_instant_forget_cycle_is_three_distinct_audit_rows() {
    let store = store();
    let forgetter = forgetter(&store);
    let fact = low_fact();
    store.insert_fact(&fact).expect("insert");
    let t0 = now();

    // forget -> unforget -> re-forget at one host instant: three real transitions,
    // three rows. The audit id is generated per applied transition, so the
    // sub-millisecond cycle can never collapse the re-forget into the first row.
    assert_eq!(
        forgetter.forget(&fact.identity.id, &t0).expect("forget"),
        PointForget::Forgotten
    );
    assert_eq!(
        forgetter
            .unforget(&fact.identity.id, &t0)
            .expect("unforget"),
        PointUnforget::Restored
    );
    assert_eq!(
        forgetter.forget(&fact.identity.id, &t0).expect("re-forget"),
        PointForget::Forgotten
    );
    assert_eq!(
        store
            .audit_by_kind(AuditKind::Forget, None, 10)
            .expect("audit")
            .events
            .len(),
        2,
        "two applied forgets, two rows — even at one instant"
    );
    assert_eq!(
        store
            .audit_by_kind(AuditKind::Unforget, None, 10)
            .expect("audit")
            .events
            .len(),
        1
    );

    // A true replay — same state, same instant — still audits nothing: idempotency
    // lives in the state gate, not the id.
    assert_eq!(
        forgetter.forget(&fact.identity.id, &t0).expect("replay"),
        PointForget::AlreadyForgotten
    );
    assert_eq!(
        store
            .audit_by_kind(AuditKind::Forget, None, 10)
            .expect("audit")
            .events
            .len(),
        2
    );
}
