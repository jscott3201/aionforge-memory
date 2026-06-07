//! End-to-end detection tests (M2.T05b, write-and-consolidation §2): the fact-extraction
//! pass detects supersession and contradiction of newly derived facts against the
//! committed current-support set, and the scheduler materializes the resulting edges in
//! the same flip transaction. These exercise the whole pipeline — extract, resolve,
//! detect, materialize — which the store-level tests (M2.T05a) cannot, since detection is
//! the pass's job, not the store's.
//!
//! Hermetic: a deterministic [`RuleExtractor`] reads the episode text and a one-hot
//! embedder keeps each distinct surface its own entity, so resolution is decidable with
//! no network. Episodes are processed in `ingested_at` order, one commit each, so a later
//! episode's detection reads the earlier one's committed facts.

use std::future::Future;
use std::sync::Arc;

use aionforge_consolidate::{
    ConsolidationConfig, Consolidator, DetectionConfig, FactExtractionPass, ObjectRule, PassConfig,
    PredicateRule, ResolutionConfig, Rule, RuleExtractor,
};
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::semantic::FactStatus;
use aionforge_domain::time::Timestamp;
use aionforge_domain::value::ObjectValue;
use aionforge_store::{BoundQuery, CandidateSet, QueryResult, Store, StoreConfig, Value};

const DIM: usize = 8;

/// A one-hot embedder: each distinct surface gets its own axis, so different surfaces are
/// orthogonal (never cluster) and an identical surface is identical (always coreferences).
/// That keeps `NYC` and `SF` distinct `Place` entities while `Alice`/`Server` reused
/// across episodes resolve to the same entity.
#[derive(Clone)]
struct AxisEmbedder {
    model: EmbedderModel,
}

impl AxisEmbedder {
    fn new() -> Self {
        Self {
            model: EmbedderModel {
                family: "axis-fake".to_string(),
                version: "1".to_string(),
                dimension: DIM as u32,
            },
        }
    }
}

fn axis(text: &str) -> usize {
    match text.trim().to_lowercase().as_str() {
        "alice" => 0,
        "nyc" => 1,
        "sf" => 2,
        "server" => 3,
        other => 4 + (other.bytes().map(usize::from).sum::<usize>() % (DIM - 4)),
    }
}

fn one_hot(index: usize) -> Embedding {
    let mut components = vec![0.0f32; DIM];
    components[index] = 1.0;
    Embedding::new(components).expect("non-empty finite vector")
}

impl Embedder for AxisEmbedder {
    type Error = std::convert::Infallible;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let out: Vec<Embedding> = inputs.iter().map(|s| one_hot(axis(s))).collect();
        async move { Ok(out) }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

fn store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: DIM as u32,
    })
    .expect("open store");
    store
        .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
        .expect("migrate store");
    Arc::new(store)
}

/// Insert a `raw` episode. `minute` makes both `ingested_at` and `captured_at` strictly
/// increasing, so discovery order (and therefore which fact is the incumbent) is fixed.
/// Trust is 0.9 — above the default high-trust threshold, so a high-trust incumbent
/// quarantines its contradiction.
fn insert_raw_episode(store: &Store, content: &str, namespace: &Namespace, minute: u32) {
    let at = ts(&format!(
        "2026-06-06T09:{minute:02}:00-05:00[America/Chicago]"
    ));
    let episode = Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: at.clone(),
            namespace: namespace.clone(),
            expired_at: None,
        },
        stats: Stats {
            importance: 0.5,
            trust: 0.9,
            last_access: at.clone(),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.0,
            is_pinned: false,
        },
        content: content.to_string(),
        role: Role::User,
        captured_at: at,
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("insert episode");
}

/// Drain every pending episode by ticking until the backlog is empty.
async fn drain(consolidator: &Consolidator) {
    loop {
        let report = consolidator.tick_once().await.expect("tick");
        if report.pending_after == 0 {
            break;
        }
        assert!(
            report.consolidated + report.retried + report.failed > 0,
            "a tick made no progress but work remains: {report:?}"
        );
    }
}

/// Count the rows a `count(...)`-style query returns as a single integer cell.
fn scalar_count(store: &Store, query: BoundQuery) -> u64 {
    match store.execute(&query).expect("count query") {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Uint(n)) => *n,
            Some(Value::Int(n)) => u64::try_from(*n).unwrap_or(0),
            other => panic!("expected an integer count, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Count `Fact` nodes with a given predicate and status.
fn facts_with_status(store: &Store, predicate: &str, status: &str) -> u64 {
    scalar_count(
        store,
        BoundQuery::new(
            "MATCH (f:Fact) WHERE f.predicate = $p AND f.status = $s RETURN count(f) AS n",
        )
        .bind_str("p", predicate)
        .expect("bind predicate")
        .bind_str("s", status)
        .expect("bind status"),
    )
}

/// Total `(:Fact)-[:label]->(:Fact)` edges in the graph.
fn total_fact_edges(store: &Store, label: &str) -> u64 {
    // gql-ident-ok: `label` is a trusted static relationship name.
    scalar_count(
        store,
        BoundQuery::new(format!(
            "MATCH (:Fact)-[r:{label}]->(:Fact) RETURN count(r) AS n"
        )),
    )
}

/// The `valid_to` of the single superseded fact's `ABOUT` window, if it is closed.
fn superseded_valid_to(store: &Store) -> Option<Timestamp> {
    let query = BoundQuery::new(
        "MATCH (f:Fact)-[r:ABOUT]->() WHERE f.status = $s RETURN r.valid_to AS valid_to LIMIT 1",
    )
    .bind_str("s", "superseded")
    .expect("bind status");
    match store.execute(&query).expect("about query") {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::ZonedDateTime(z)) => Some((**z).clone()),
            _ => None,
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The statuses of the current-support facts (no live `SUPERSEDED_BY`) carrying a given
/// predicate — the §9 set the retrieval path treats as "current".
fn current_support_with_predicate(store: &Store, predicate: &str) -> Vec<FactStatus> {
    let members = store
        .candidate_state_members(CandidateSet::CurrentSupportFacts)
        .expect("current-support members");
    let mut out = Vec::new();
    for node in members {
        if let Some(fact) = store.fact_by_node_id(node).expect("fact by node")
            && fact.predicate == predicate
        {
            out.push(fact.status);
        }
    }
    out
}

#[tokio::test]
async fn a_functional_predicate_supersedes_across_episodes() {
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());

    // E1 asserts Alice is based in NYC; a later E2 asserts SF. `based_in` is functional in
    // the default ruleset, so the newer assertion supersedes the older one.
    insert_raw_episode(&store, "Alice is based in NYC.", &namespace, 0);
    insert_raw_episode(&store, "Alice is based in SF.", &namespace, 5);

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(AxisEmbedder::new()),
        PassConfig::default(),
    )));

    drain(&consolidator).await;

    // Both facts are preserved — supersession is non-destructive — but with distinct states.
    assert_eq!(
        facts_with_status(&store, "based_in", "superseded"),
        1,
        "the NYC fact is retired, not deleted"
    );
    assert_eq!(
        facts_with_status(&store, "based_in", "active"),
        1,
        "the SF fact is the live one"
    );
    assert_eq!(
        total_fact_edges(&store, "SUPERSEDED_BY"),
        1,
        "exactly one supersession edge links old to new"
    );

    // The retired fact's event-time window closes at the superseding episode's event time.
    assert_eq!(
        superseded_valid_to(&store),
        Some(ts("2026-06-06T09:05:00-05:00[America/Chicago]")),
        "the prior window closes at E2's captured_at"
    );

    // The current-support set now holds only the live SF fact.
    let current = current_support_with_predicate(&store, "based_in");
    assert_eq!(
        current,
        vec![FactStatus::Active],
        "current support is exactly the one active SF fact: {current:?}"
    );
}

#[tokio::test]
async fn a_high_trust_contradiction_quarantines_the_new_fact() {
    let store = store();
    let namespace = Namespace::Agent("ops".to_string());

    // A single-rule extractor for a `status` predicate whose object is a free-text literal
    // ("up"/"down"), paired with a detection config that declares those two mutually
    // exclusive. The rule extractor cannot emit booleans (M4's model-backed one will), so a
    // configured antonym pair stands in for the always-on boolean-inversion rule here.
    let extractor = RuleExtractor::new(
        "rule-status",
        vec![Rule {
            marker: "status".to_string(),
            predicate: "status".to_string(),
            subject_type: "Service".to_string(),
            object: ObjectRule::Text,
            confidence: 0.9,
        }],
    );
    let mut detection = DetectionConfig::with_default_rules();
    detection.predicates.insert(
        "status".to_string(),
        PredicateRule {
            functional: false,
            contradicts: vec![(
                ObjectValue::Text("up".to_string()),
                ObjectValue::Text("down".to_string()),
            )],
        },
    );
    let pass_config = PassConfig {
        resolution: ResolutionConfig::default(),
        detection,
    };

    // E1 (high-trust, 0.9) says the server is up; E2 says it is down — a contradiction.
    insert_raw_episode(&store, "Server status up.", &namespace, 0);
    insert_raw_episode(&store, "Server status down.", &namespace, 5);

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(extractor),
        Arc::new(AxisEmbedder::new()),
        pass_config,
    )));

    drain(&consolidator).await;

    // The incumbent "up" fact is untouched; the new "down" fact is quarantined.
    assert_eq!(
        facts_with_status(&store, "status", "active"),
        1,
        "the high-trust incumbent stays active"
    );
    assert_eq!(
        facts_with_status(&store, "status", "quarantined"),
        1,
        "the contradicting new fact is held for review"
    );
    assert_eq!(
        total_fact_edges(&store, "CONTRADICTS"),
        1,
        "one CONTRADICTS edge from the new fact to the incumbent"
    );

    // The contradiction is not a supersession: nothing is retired.
    assert_eq!(
        facts_with_status(&store, "status", "superseded"),
        0,
        "a contradiction does not retire either fact"
    );

    // A quarantine reconcile signal is recorded for review.
    assert_eq!(
        scalar_count(
            &store,
            BoundQuery::new("MATCH (a:AuditEvent) WHERE a.kind = $k RETURN count(a) AS n")
                .bind_str("k", "quarantine")
                .expect("bind kind"),
        ),
        1,
        "the quarantine raises exactly one audit event"
    );
}
