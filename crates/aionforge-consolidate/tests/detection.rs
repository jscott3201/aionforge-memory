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
    ConsolidationConfig, Consolidator, DetectionConfig, FactExtractionPass, InductionConfig,
    ObjectRule, PassConfig, PredicateRule, ResolutionConfig, Rule, RuleExtractor, RuleSummarizer,
    SummarizationConfig,
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

const DIM: usize = 12;

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
        "boston" => 4,
        "rust" => 5,
        "go" => 6,
        other => 7 + (other.bytes().map(usize::from).sum::<usize>() % (DIM - 7)),
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

/// Insert a `raw` episode at trust 0.9 — above the default high-trust threshold, so a
/// high-trust incumbent quarantines its contradiction.
fn insert_raw_episode(store: &Store, content: &str, namespace: &Namespace, minute: u32) {
    insert_raw_episode_trust(store, content, namespace, minute, 0.9);
}

/// Insert a `raw` episode with an explicit trust. `minute` makes both `ingested_at` and
/// `captured_at` strictly increasing, so discovery order (and therefore which fact is the
/// incumbent) is fixed; `trust` flows onto the derived fact and drives the quarantine
/// decision when it is the incumbent of a contradiction.
fn insert_raw_episode_trust(
    store: &Store,
    content: &str,
    namespace: &Namespace,
    minute: u32,
    trust: f64,
) {
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
            trust,
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

/// The `valid_from` of the single active fact's `ABOUT` window for a predicate — which event
/// (by its captured time) is the live one, independent of the resolved entity object.
fn active_valid_from(store: &Store, predicate: &str) -> Option<Timestamp> {
    let query = BoundQuery::new(
        "MATCH (f:Fact)-[r:ABOUT]->() WHERE f.predicate = $p AND f.status = $s \
         RETURN r.valid_from AS valid_from LIMIT 1",
    )
    .bind_str("p", predicate)
    .expect("bind predicate")
    .bind_str("s", "active")
    .expect("bind status");
    match store.execute(&query).expect("about query") {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::ZonedDateTime(z)) => Some((**z).clone()),
            _ => None,
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Insert a `raw` episode with independently set arrival (`ingested`) and event (`captured`)
/// minutes, so a backfilled past event can be made to arrive *after* a newer one — the case
/// that exercises arrival order contradicting event order. Trust 0.9 (high), like the
/// default helper.
fn insert_raw_episode_backdated(
    store: &Store,
    content: &str,
    namespace: &Namespace,
    ingested_minute: u32,
    captured_minute: u32,
) {
    let ingested = ts(&format!(
        "2026-06-06T09:{ingested_minute:02}:00-05:00[America/Chicago]"
    ));
    let captured = ts(&format!(
        "2026-06-06T09:{captured_minute:02}:00-05:00[America/Chicago]"
    ));
    let episode = Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ingested.clone(),
            namespace: namespace.clone(),
            expired_at: None,
        },
        stats: Stats {
            importance: 0.5,
            trust: 0.9,
            last_access: ingested,
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.0,
            is_pinned: false,
        },
        content: content.to_string(),
        role: Role::User,
        captured_at: captured,
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

/// The objects of the current-support facts (the recall set) for a predicate, sorted so the
/// result is comparable across runs regardless of node iteration order.
fn current_support_objects(store: &Store, predicate: &str) -> Vec<ObjectValue> {
    let members = store
        .candidate_state_members(CandidateSet::CurrentSupportFacts)
        .expect("current-support members");
    let mut out = Vec::new();
    for node in members {
        if let Some(fact) = store.fact_by_node_id(node).expect("fact by node")
            && fact.predicate == predicate
        {
            out.push(fact.object);
        }
    }
    out.sort_by_key(|object| format!("{object:?}"));
    out
}

/// Reset every episode to `raw` so a re-drain replays consolidation — the crash/re-trigger
/// path. With content-derived ids and idempotent materialization, a replay writes nothing.
fn reset_to_raw(store: &Store) {
    let query =
        BoundQuery::new("MATCH (e:Episode) SET e.consolidation_state = $raw RETURN e.id AS id")
            .bind_str("raw", "raw")
            .expect("bind raw");
    store.execute(&query).expect("reset episodes to raw");
}

/// Count `AuditEvent` nodes of a given kind.
fn audit_count(store: &Store, kind: &str) -> u64 {
    scalar_count(
        store,
        BoundQuery::new("MATCH (a:AuditEvent) WHERE a.kind = $k RETURN count(a) AS n")
            .bind_str("k", kind)
            .expect("bind kind"),
    )
}

/// A single-rule `status` extractor (free-text object) plus a detection config that
/// declares "up"/"down" mutually exclusive — the configured-antonym stand-in for the
/// always-on boolean inversion (the rule extractor cannot emit booleans; M4's will).
fn status_extractor_and_config() -> (RuleExtractor, PassConfig) {
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
    (
        extractor,
        PassConfig {
            resolution: ResolutionConfig::default(),
            detection,
            summarization: SummarizationConfig::default(),
            induction: InductionConfig::default(),
            ..PassConfig::default()
        },
    )
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
        Arc::new(RuleSummarizer::with_default_rules()),
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
async fn a_backfilled_stale_episode_still_converges_to_the_newer_value() {
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());

    // SF is the NEWER event (captured 09:05) but is ingested FIRST (arrival 09:00), so it is
    // consolidated first and becomes the incumbent. NYC is an OLDER event (captured 09:00)
    // backfilled LATER (arrival 09:05), consolidated second — a stale assertion arriving
    // after a newer incumbent, with arrival order deliberately contradicting event order.
    // The mirror image of `a_functional_predicate_supersedes_across_episodes`, and it must
    // reach the SAME current state (SF live, NYC retired): convergence under reordering.
    insert_raw_episode_backdated(&store, "Alice is based in SF.", &namespace, 0, 5);
    insert_raw_episode_backdated(&store, "Alice is based in NYC.", &namespace, 5, 0);

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(AxisEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        PassConfig::default(),
    )));

    drain(&consolidator).await;

    // Exactly one current value — not two. The old forward-only guard left the stale NYC
    // fact additive (a second active value) when it arrived after the newer incumbent.
    assert_eq!(
        facts_with_status(&store, "based_in", "active"),
        1,
        "one current value even when the stale event is consolidated last"
    );
    assert_eq!(
        facts_with_status(&store, "based_in", "superseded"),
        1,
        "the stale NYC assertion is retired into history, not left additive"
    );
    assert_eq!(
        total_fact_edges(&store, "SUPERSEDED_BY"),
        1,
        "the stale fact points at the surviving incumbent"
    );

    // The survivor is SF — the newer event — identified by its event-time window opening at
    // 09:05, regardless of the reversed arrival order.
    assert_eq!(
        active_valid_from(&store, "based_in"),
        Some(ts("2026-06-06T09:05:00-05:00[America/Chicago]")),
        "the live value is the newer (SF) event, not the stale NYC one"
    );
    // The stale fact's window closes at the incumbent's valid_from (09:05), so NYC is held as
    // the past value [09:00, 09:05) — recoverable, never lost.
    assert_eq!(
        superseded_valid_to(&store),
        Some(ts("2026-06-06T09:05:00-05:00[America/Chicago]")),
        "the stale NYC window closes at the newer event's valid_from"
    );
    let current = current_support_with_predicate(&store, "based_in");
    assert_eq!(
        current,
        vec![FactStatus::Active],
        "current support converges to the single newer value: {current:?}"
    );
}

#[tokio::test]
async fn a_high_trust_contradiction_quarantines_the_new_fact() {
    let store = store();
    let namespace = Namespace::Agent("ops".to_string());

    let (extractor, pass_config) = status_extractor_and_config();

    // E1 (high-trust, 0.9) says the server is up; E2 says it is down — a contradiction.
    insert_raw_episode(&store, "Server status up.", &namespace, 0);
    insert_raw_episode(&store, "Server status down.", &namespace, 5);

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(extractor),
        Arc::new(AxisEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        pass_config,
    )));

    drain(&consolidator).await;

    // Both facts are high-trust (0.9), so the quarantine victim is settled by the object-order
    // tiebreak: 'down' sorts before 'up', so 'down' is the victim — here that is the new fact,
    // but by the symmetric rule, not by virtue of arriving second. The "up" survivor stays
    // active; the "down" victim is quarantined.
    assert_eq!(
        facts_with_status(&store, "status", "active"),
        1,
        "the surviving 'up' fact stays active"
    );
    assert_eq!(
        facts_with_status(&store, "status", "quarantined"),
        1,
        "the 'down' victim is held for review"
    );
    assert_eq!(
        total_fact_edges(&store, "CONTRADICTS"),
        1,
        "one CONTRADICTS edge from the victim to the survivor"
    );

    // The contradiction is not a supersession: nothing is retired.
    assert_eq!(
        facts_with_status(&store, "status", "superseded"),
        0,
        "a contradiction does not retire either fact"
    );

    // A quarantine reconcile signal is recorded for review.
    assert_eq!(
        audit_count(&store, "quarantine"),
        1,
        "the quarantine raises exactly one audit event"
    );
}

#[tokio::test]
async fn multiple_new_values_for_one_functional_predicate_route_to_one_survivor() {
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());

    // E1 establishes the NYC incumbent; E2 asserts TWO competing values for the same
    // functional predicate in one episode (SF and Boston). The episode's intra-episode
    // tiebreak keeps one survivor, and every retirement — the incumbent and the losing
    // peer — routes to that single survivor.
    insert_raw_episode(&store, "Alice is based in NYC.", &namespace, 0);
    insert_raw_episode(
        &store,
        "Alice is based in SF. Alice is based in Boston.",
        &namespace,
        5,
    );

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(AxisEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        PassConfig::default(),
    )));

    drain(&consolidator).await;

    // Two of the three facts are retired (NYC and the losing peer); exactly one stays live.
    assert_eq!(
        facts_with_status(&store, "based_in", "superseded"),
        2,
        "the incumbent and the losing peer are both retired"
    );
    assert_eq!(
        facts_with_status(&store, "based_in", "active"),
        1,
        "a functional predicate keeps exactly one current value"
    );
    // Two SUPERSEDED_BY edges, both terminating at the one survivor (so it has no outgoing
    // SUPERSEDED_BY of its own) — never the redundant, differently-targeted topology.
    assert_eq!(
        total_fact_edges(&store, "SUPERSEDED_BY"),
        2,
        "each retired fact points at the survivor exactly once"
    );
    let current = current_support_with_predicate(&store, "based_in");
    assert_eq!(
        current,
        vec![FactStatus::Active],
        "current support is exactly the one surviving fact: {current:?}"
    );
}

#[tokio::test]
async fn replaying_an_episode_re_applies_no_detection() {
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());

    insert_raw_episode(&store, "Alice is based in NYC.", &namespace, 0);
    insert_raw_episode(&store, "Alice is based in SF.", &namespace, 5);

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(AxisEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        PassConfig::default(),
    )));

    drain(&consolidator).await;

    // Snapshot the post-consolidation shape.
    let superseded = facts_with_status(&store, "based_in", "superseded");
    let active = facts_with_status(&store, "based_in", "active");
    let edges = total_fact_edges(&store, "SUPERSEDED_BY");
    assert_eq!((superseded, active, edges), (1, 1, 1), "first pass result");

    // Replay every episode. Content-derived ids reuse the same fact nodes, the survivor's
    // window is already closed, and the supersession edge already exists — so detection
    // emits the same instruction and materialization applies nothing new.
    reset_to_raw(&store);
    drain(&consolidator).await;

    assert_eq!(
        facts_with_status(&store, "based_in", "superseded"),
        superseded,
        "replay adds no superseded fact"
    );
    assert_eq!(
        facts_with_status(&store, "based_in", "active"),
        active,
        "replay adds no active fact"
    );
    assert_eq!(
        total_fact_edges(&store, "SUPERSEDED_BY"),
        edges,
        "replay adds no second SUPERSEDED_BY edge"
    );
    let current = current_support_with_predicate(&store, "based_in");
    assert_eq!(
        current,
        vec![FactStatus::Active],
        "current support is unchanged after replay: {current:?}"
    );
}

#[tokio::test]
async fn a_symmetric_low_trust_contradiction_keeps_both_active_but_recall_picks_one() {
    let store = store();
    let namespace = Namespace::Agent("ops".to_string());
    let (extractor, pass_config) = status_extractor_and_config();

    // Both sides are LOW trust (0.5, below the 0.7 bar): the contradiction is recorded and
    // neither is quarantined (status stays active for both). But recall still resolves to one
    // value — the CONTRADICTS source (the victim, chosen symmetrically by the smaller object
    // order) is excluded from current_support by edge presence, regardless of status.
    insert_raw_episode_trust(&store, "Server status up.", &namespace, 0, 0.5);
    insert_raw_episode_trust(&store, "Server status down.", &namespace, 5, 0.5);

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(extractor),
        Arc::new(AxisEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        pass_config,
    )));

    drain(&consolidator).await;

    // Both facts stay active by status, and nothing is quarantined.
    assert_eq!(
        facts_with_status(&store, "status", "active"),
        2,
        "neither fact is held back below the trust threshold"
    );
    assert_eq!(
        facts_with_status(&store, "status", "quarantined"),
        0,
        "below the trust bar, neither side is quarantined"
    );
    assert_eq!(
        total_fact_edges(&store, "CONTRADICTS"),
        1,
        "the contradiction is still recorded as an edge"
    );
    assert_eq!(
        audit_count(&store, "quarantine"),
        0,
        "no quarantine, so no reconcile signal"
    );
    // The recall set is exactly one fact: the contradiction's victim is excluded from
    // current_support even though its status is active — the observable point of the
    // symmetric-victim rule that the status-only assertions miss.
    let current = current_support_with_predicate(&store, "status");
    assert_eq!(
        current,
        vec![FactStatus::Active],
        "recall resolves the contradiction to a single value: {current:?}"
    );
}

/// Consolidate an up/down `status` contradiction with the given per-side trust in a chosen
/// arrival order, and return the recall set (current-support objects) for `status`.
async fn recall_after_status_contradiction(
    up_trust: f64,
    down_trust: f64,
    up_first: bool,
) -> Vec<ObjectValue> {
    let store = store();
    let namespace = Namespace::Agent("ops".to_string());
    let (extractor, pass_config) = status_extractor_and_config();

    let (first, first_trust, second, second_trust) = if up_first {
        (
            "Server status up.",
            up_trust,
            "Server status down.",
            down_trust,
        )
    } else {
        (
            "Server status down.",
            down_trust,
            "Server status up.",
            up_trust,
        )
    };
    insert_raw_episode_trust(&store, first, &namespace, 0, first_trust);
    insert_raw_episode_trust(&store, second, &namespace, 5, second_trust);

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(extractor),
        Arc::new(AxisEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        pass_config,
    )));
    drain(&consolidator).await;
    current_support_objects(&store, "status")
}

#[tokio::test]
async fn a_contradiction_converges_to_the_same_recall_in_either_arrival_order() {
    let up = ObjectValue::Text("up".to_string());

    // Asymmetric trust: the lower-trust 'down' is the victim, so 'up' survives recall — in BOTH
    // arrival orders. This is the convergence the arrival-fragile incumbent-keyed rule lacked
    // (it left recall depending on which side was consolidated first).
    let asym_up_first = recall_after_status_contradiction(0.9, 0.5, true).await;
    let asym_down_first = recall_after_status_contradiction(0.9, 0.5, false).await;
    assert_eq!(
        asym_up_first,
        vec![up.clone()],
        "the higher-trust 'up' survives"
    );
    assert_eq!(
        asym_up_first, asym_down_first,
        "recall is identical regardless of arrival order"
    );

    // Symmetric (both-low) trust: the victim is the smaller object order ('down'), so 'up'
    // survives — again identical across orders.
    let sym_up_first = recall_after_status_contradiction(0.5, 0.5, true).await;
    let sym_down_first = recall_after_status_contradiction(0.5, 0.5, false).await;
    assert_eq!(
        sym_up_first,
        vec![up],
        "the larger-object-order 'up' survives the tie"
    );
    assert_eq!(
        sym_up_first, sym_down_first,
        "recall is identical regardless of arrival order"
    );
}

#[tokio::test]
async fn a_multi_valued_predicate_keeps_every_value() {
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());

    // `prefers` is unregistered, so it is multi-valued: a second preference is additive, not
    // a supersession or a contradiction. This is the conservative-by-default invariant end
    // to end — extraction, resolution, and materialization all leave both facts live.
    insert_raw_episode(&store, "Alice prefers Rust.", &namespace, 0);
    insert_raw_episode(&store, "Alice prefers Go.", &namespace, 5);

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(AxisEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        PassConfig::default(),
    )));

    drain(&consolidator).await;

    assert_eq!(
        facts_with_status(&store, "prefers", "active"),
        2,
        "both preferences are retained"
    );
    assert_eq!(
        total_fact_edges(&store, "SUPERSEDED_BY"),
        0,
        "an additive predicate retires nothing"
    );
    assert_eq!(
        total_fact_edges(&store, "CONTRADICTS"),
        0,
        "independent values do not contradict"
    );
    let current = current_support_with_predicate(&store, "prefers");
    assert_eq!(
        current,
        vec![FactStatus::Active, FactStatus::Active],
        "current support holds both live preferences: {current:?}"
    );
}
