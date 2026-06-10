//! Integration tests for the importance and recency re-rank signals (M5.T01, 05 §2, 03 §1).
//!
//! Both are re-ranks, not retrievals: they order the candidates the search signals already
//! surfaced — importance by the *effective* (decayed) value computed at rank time from the
//! caller-supplied clock, recency by the immutable ingestion instant — and fold into
//! reciprocal-rank fusion under their own signals. These tests pin: the clock gate (no
//! `options.now`, no re-rank — a clockless recall is unchanged); raw-importance ordering with
//! decay off; the decay effect (a stale memory sinks below a fresh peer) and its pin exemption
//! (a pinned stale memory never sinks); the recency lift on episodes; the uniform-set collapse
//! (equal values reorder nothing — the competition rank end-to-end); and the weight gate (the
//! quote class runs neither).
//!
//! Hermetic, mirroring the trust re-rank suite: a fake embedder maps the query and every body
//! to one fixed vector, so all candidates tie on dense and the base signals separate them only
//! by node id (creation order). A winner is always created *last* (worst base rank), so a climb
//! is attributable to the signal under test; trust is uniform and ingestion instants are held
//! equal except where recency itself is under test.

use std::future::Future;
use std::sync::Arc;

use aionforge_domain::Retriever;
use aionforge_domain::authz::Principal;
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::edges::About;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::value::ObjectValue;
use aionforge_retrieval::{
    HybridRetriever, QueryClass, RecallBundle, RecallOptions, RecallQuery, RetrieverConfig, Signal,
    StructuredEntry,
};
use aionforge_store::{NodeId, Store, StoreConfig};

const QUERY: &str = "the recurring topic";
const NEAR: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
const FAR: [f32; 4] = [0.0, 1.0, 0.0, 0.0];
const HOUR_SECS: f64 = 3_600.0;

/// A vector near the query but *distinct per item*: the substrate's approximate vector
/// index can drop exact-duplicate points (a duplicate inserted last may be unreachable in
/// the proximity graph), so every candidate gets its own slightly-off-query vector. Higher
/// `i` sits farther from the query, so a test's winner takes the LARGEST `i` — the worst
/// dense rank — and a climb stays attributable to the signal under test.
fn near(i: u32) -> [f32; 4] {
    [1.0, 0.1 * (i + 1) as f32, 0.0, 0.0]
}

#[derive(Clone)]
struct FakeEmbedder {
    model: EmbedderModel,
}

impl FakeEmbedder {
    fn new() -> Self {
        Self {
            model: EmbedderModel {
                family: "fake".to_string(),
                version: "1".to_string(),
                dimension: 4,
            },
        }
    }
}

#[derive(Debug)]
struct FakeEmbedError;
impl std::fmt::Display for FakeEmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unreachable")
    }
}
impl std::error::Error for FakeEmbedError {}

impl Embedder for FakeEmbedder {
    type Error = FakeEmbedError;
    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let out = inputs
            .iter()
            .map(|_| Embedding::new(NEAR.to_vec()).expect("valid"))
            .collect();
        async move { Ok(out) }
    }
    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

/// `T0 + hours`, the test's single time axis.
fn at(hours: u32) -> Timestamp {
    format!("2026-01-01T{hours:02}:00:00Z[UTC]")
        .parse()
        .expect("valid zoned datetime")
}

fn store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store.migrate(&at(0)).expect("migrate store");
    Arc::new(store)
}

/// Uniform trust, parameterized importance/access/pin — the axes these tests move.
fn stats(importance: f64, last_access: Timestamp, is_pinned: bool) -> Stats {
    Stats {
        importance,
        trust: 0.5,
        last_access,
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned,
    }
}

fn identity_at(ingested_at: Timestamp) -> Identity {
    Identity {
        id: Id::generate(),
        ingested_at,
        namespace: Namespace::Global,
        expired_at: None,
    }
}

/// The corpus's single shared subject entity, at the FAR vector with a name the query
/// never uses. Shared deliberately: the high-precision path resolves the query's
/// nearest entities by ANN (top-5, distance-unbounded) and scopes the fact dense search
/// to THEIR facts — one subject keeps that seed covering the whole corpus, where
/// per-fact subjects would silently exclude every fact past the fifth entity.
fn subject(store: &Store) -> (Id, NodeId) {
    let identity = identity_at(at(0));
    let id = identity.id;
    let ent = Entity {
        identity,
        stats: stats(0.5, at(0), false),
        canonical_name: format!("unrelated subject {id}"),
        entity_type: "Concept".to_string(),
        aliases: vec![],
        description: None,
        embedding: Some(Embedding::new(FAR.to_vec()).expect("valid")),
        embedder_model: None,
        attributes: None,
    };
    let node = store.insert_entity(&ent).expect("insert entity");
    (id, node)
}

/// Assert a near-query fact whose `stats` carry the axis under test. All facts ingest at
/// `T0`, so the recency signal stays uniform across them and only importance can reorder.
fn fact_with_stats(
    store: &Store,
    subject: &(Id, NodeId),
    statement: &str,
    stats: Stats,
    vector: [f32; 4],
) {
    let (subject_id, subject_node) = *subject;
    let f = Fact {
        identity: identity_at(at(0)),
        stats,
        subject_id,
        predicate: "rel".to_string(),
        object: ObjectValue::Text(statement.to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: statement.to_string(),
        embedding: Some(Embedding::new(vector.to_vec()).expect("valid")),
        embedder_model: None,
        extraction: None,
    };
    let about = About {
        temporal: BiTemporal {
            valid_from: at(0),
            valid_to: None,
            ingested_at: at(0),
            expired_at: None,
        },
    };
    store
        .assert_fact(&f, subject_node, &about)
        .expect("assert fact");
}

/// Insert a near-query episode ingested at `ingested_at`, everything else uniform — only
/// the recency signal can separate two of these.
fn episode_ingested_at(store: &Store, content: &str, ingested_at: Timestamp, seed: u128) {
    episode_with_stats(store, content, stats(0.5, at(0), false), ingested_at, seed);
}

/// Insert a near-query episode whose `stats` carry the axis under test.
fn episode_with_stats(
    store: &Store,
    content: &str,
    stats: Stats,
    ingested_at: Timestamp,
    seed: u128,
) {
    let episode = Episode {
        identity: identity_at(ingested_at),
        stats,
        content: content.to_string(),
        role: Role::User,
        captured_at: at(0),
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(&seed.to_le_bytes()),
        embedding: Some(
            Embedding::new(near(u32::try_from(seed).expect("small seed")).to_vec()).expect("valid"),
        ),
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("insert episode");
}

fn retriever(store: Arc<Store>, config: RetrieverConfig) -> HybridRetriever<FakeEmbedder> {
    HybridRetriever::new(store, FakeEmbedder::new(), config)
}

/// A one-hour semantic half-life so a six-hour-stale fact visibly sinks.
fn decay_on() -> RetrieverConfig {
    RetrieverConfig {
        decay_enabled: true,
        episodic_half_life_secs: HOUR_SECS,
        semantic_half_life_secs: HOUR_SECS,
        ..RetrieverConfig::default()
    }
}

/// Split half-lives — episodic one hour, semantic a thousand — so the two tier-pinning
/// tests can tell WHICH half-life each kind reads: a six-hour-stale memory decays to
/// nothing on the episodic clock and to ~0.997 of stored on the semantic one.
fn decay_on_split() -> RetrieverConfig {
    RetrieverConfig {
        decay_enabled: true,
        episodic_half_life_secs: HOUR_SECS,
        semantic_half_life_secs: 1_000.0 * HOUR_SECS,
        ..RetrieverConfig::default()
    }
}

async fn recall(
    r: &HybridRetriever<FakeEmbedder>,
    class: QueryClass,
    now: Option<Timestamp>,
) -> RecallBundle {
    r.recall(RecallQuery {
        text: QUERY.to_string(),
        principal: Principal::agent(Id::generate()),
        limit: 20,
        options: RecallOptions {
            mode_override: Some(class),
            fanout: 20,
            now,
            ..RecallOptions::default()
        },
    })
    .await
    .expect("recall")
}

fn fact_signals(bundle: &RecallBundle, statement: &str) -> Option<Vec<Signal>> {
    bundle.structured.iter().find_map(|e| match e {
        StructuredEntry::Fact(f) if f.statement == statement => {
            Some(f.contributions.iter().map(|c| c.signal).collect())
        }
        _ => None,
    })
}

fn fact_rank(bundle: &RecallBundle, statement: &str) -> Option<usize> {
    bundle
        .structured
        .iter()
        .position(|e| matches!(e, StructuredEntry::Fact(f) if f.statement == statement))
}

fn episode_rank(bundle: &RecallBundle, content: &str) -> Option<usize> {
    bundle
        .structured
        .iter()
        .position(|e| matches!(e, StructuredEntry::Episode(ep) if ep.content == content))
}

fn episode_signals(bundle: &RecallBundle, content: &str) -> Option<Vec<Signal>> {
    bundle.structured.iter().find_map(|e| match e {
        StructuredEntry::Episode(ep) if ep.content == content => {
            Some(ep.contributions.iter().map(|c| c.signal).collect())
        }
        _ => None,
    })
}

fn statement_order(bundle: &RecallBundle) -> Vec<String> {
    bundle
        .structured
        .iter()
        .filter_map(|e| match e {
            StructuredEntry::Fact(f) => Some(f.statement.clone()),
            StructuredEntry::Episode(_) => None,
        })
        .collect()
}

const LAST_LOW: &str = "low importance claim 4";
const HIGH: &str = "high importance claim";

/// Five lower-importance facts created first (so they hold the better base node-id ranks) at
/// distinct descending importances, then one high-importance fact created last (worst base
/// rank). With decay off, only the raw stored importance can lift it.
fn importance_corpus() -> Arc<Store> {
    let store = store();
    let subj = subject(&store);
    for i in 0..5 {
        fact_with_stats(
            &store,
            &subj,
            &format!("low importance claim {i}"),
            stats(0.50 - 0.10 * f64::from(i), at(0), false),
            near(i),
        );
    }
    fact_with_stats(&store, &subj, HIGH, stats(0.95, at(0), false), near(5));
    store
}

#[tokio::test]
async fn the_re_ranks_are_absent_without_a_clock() {
    // The clock gate: `options.now` is None (the default), so neither re-rank exists — the
    // recall is byte-identical to a pre-decay one even though the profile weights both.
    let bundle = recall(
        &retriever(importance_corpus(), RetrieverConfig::default()),
        QueryClass::SingleHopFactual,
        None,
    )
    .await;

    for signal in [Signal::Importance, Signal::Recency] {
        assert!(
            !bundle.explanation.signals_run.contains(&signal),
            "{signal:?} must not run without a caller clock"
        );
    }
    let high = fact_signals(&bundle, HIGH).expect("high-importance fact surfaced");
    assert!(
        !high.contains(&Signal::Importance) && !high.contains(&Signal::Recency),
        "no candidate carries a clockless re-rank contribution: {high:?}"
    );
}

#[tokio::test]
async fn the_importance_re_rank_lifts_a_high_importance_fact() {
    // Decay off: the re-rank orders by the raw stored importance. The high-importance fact is
    // created last, so every base signal ranks it dead last by node id; recency is uniform
    // (all facts ingest at T0) and trust is uniform — only importance can lift it.
    let bundle = recall(
        &retriever(importance_corpus(), RetrieverConfig::default()),
        QueryClass::SingleHopFactual,
        Some(at(6)),
    )
    .await;

    let high_rank = fact_rank(&bundle, HIGH).expect("high-importance fact surfaced");
    let low_rank = fact_rank(&bundle, LAST_LOW).expect("low-importance fact surfaced");
    assert!(
        high_rank < low_rank,
        "the high-importance fact (worst base rank) climbs over a low-importance peer purely \
         on importance (high #{high_rank}, low #{low_rank})"
    );
    assert!(
        fact_signals(&bundle, HIGH).is_some_and(|s| s.contains(&Signal::Importance)),
        "the surfaced fact carries an Importance contribution"
    );
    assert!(
        bundle.explanation.signals_run.contains(&Signal::Importance),
        "the importance signal is reported as run"
    );
}

#[tokio::test]
async fn decay_sinks_a_stale_memory_below_a_fresh_one() {
    // Equal stored importance, one-hour half-life: the stale fact (last accessed at T0, six
    // half-lives before `now`) decays to ~0.0125 effective while the fresh one (just accessed)
    // holds 0.8. The fresh fact is created last — worst base rank — so its climb is purely the
    // decayed-importance effect. With decay off these two are uniform and nothing reorders.
    let store = store();
    let subj = subject(&store);
    fact_with_stats(
        &store,
        &subj,
        "stale claim",
        stats(0.8, at(0), false),
        near(0),
    );
    fact_with_stats(
        &store,
        &subj,
        "fresh claim",
        stats(0.8, at(6), false),
        near(1),
    );
    // Four freshly-accessed spreaders at distinct mid importances: a LIGHT-weighted re-rank
    // cannot flip an adjacent dense pair (deliberate — see the trust re-rank), so the climb
    // needs the stale fact pushed multiple importance positions below the fresh one.
    for i in 0..4u32 {
        fact_with_stats(
            &store,
            &subj,
            &format!("spreader claim {i}"),
            stats(0.7 - 0.1 * f64::from(i), at(6), false),
            near(i + 2),
        );
    }

    let bundle = recall(
        &retriever(store, decay_on()),
        QueryClass::SingleHopFactual,
        Some(at(6)),
    )
    .await;

    let fresh = fact_rank(&bundle, "fresh claim").expect("fresh fact surfaced");
    let stale = fact_rank(&bundle, "stale claim").expect("stale fact surfaced");
    assert!(
        fresh < stale,
        "the freshly-accessed fact outranks the six-half-lives-stale one (fresh #{fresh}, \
         stale #{stale})"
    );
    assert!(
        fact_signals(&bundle, "stale claim").is_some_and(|s| s.contains(&Signal::Importance)),
        "the stale fact still carries an Importance contribution — it sank, it was not dropped"
    );
}

#[tokio::test]
async fn a_pinned_stale_memory_never_sinks() {
    // The pin exemption, end to end: the pinned fact is just as stale as the sinking fact in
    // the test above AND created last (worst base rank), but the pin holds it at its full
    // stored importance 0.8 against the fresh fact's 0.5 — so it climbs. Without the pin
    // short-circuit it would decay to ~0.0125 and sink below the fresh fact instead.
    let store = store();
    let subj = subject(&store);
    fact_with_stats(
        &store,
        &subj,
        "fresh claim",
        stats(0.3, at(6), false),
        near(0),
    );
    fact_with_stats(
        &store,
        &subj,
        "pinned stale claim",
        stats(0.8, at(0), true),
        near(1),
    );
    for i in 0..4u32 {
        fact_with_stats(
            &store,
            &subj,
            &format!("spreader claim {i}"),
            stats(0.7 - 0.1 * f64::from(i), at(6), false),
            near(i + 2),
        );
    }

    let bundle = recall(
        &retriever(store, decay_on()),
        QueryClass::SingleHopFactual,
        Some(at(6)),
    )
    .await;

    let pinned = fact_rank(&bundle, "pinned stale claim").expect("pinned fact surfaced");
    let fresh = fact_rank(&bundle, "fresh claim").expect("fresh fact surfaced");
    assert!(
        pinned < fresh,
        "the pinned stale fact keeps its full importance and outranks the fresher, \
         lower-importance peer (pinned #{pinned}, fresh #{fresh})"
    );
}

#[tokio::test]
async fn facts_decay_on_the_semantic_half_life_not_the_episodic_one() {
    // The tier pin, fact side: same corpus and geometry as the sink test above, but under
    // SPLIT half-lives (episodic 1h, semantic 1000h). Facts must read the semantic clock,
    // so the six-hour-stale fact keeps ~0.997 of its importance and its better base rank —
    // it does NOT sink. A tier mapping inverted to the episodic clock decays it to ~0.0125
    // and the fresh fact climbs, failing this assertion.
    let store = store();
    let subj = subject(&store);
    fact_with_stats(
        &store,
        &subj,
        "stale claim",
        stats(0.8, at(0), false),
        near(0),
    );
    fact_with_stats(
        &store,
        &subj,
        "fresh claim",
        stats(0.8, at(6), false),
        near(1),
    );
    for i in 0..4u32 {
        fact_with_stats(
            &store,
            &subj,
            &format!("spreader claim {i}"),
            stats(0.7 - 0.1 * f64::from(i), at(6), false),
            near(i + 2),
        );
    }

    let bundle = recall(
        &retriever(store, decay_on_split()),
        QueryClass::SingleHopFactual,
        Some(at(6)),
    )
    .await;

    let stale = fact_rank(&bundle, "stale claim").expect("stale fact surfaced");
    let fresh = fact_rank(&bundle, "fresh claim").expect("fresh fact surfaced");
    assert!(
        stale < fresh,
        "on the semantic clock six hours is nothing: the stale fact keeps its base rank          (stale #{stale}, fresh #{fresh})"
    );
}

#[tokio::test]
async fn episodes_decay_on_the_episodic_half_life() {
    // The tier pin, episode side: the same split half-lives, an episode corpus, and the
    // recall (temporal) class. Episodes must read the EPISODIC clock, so the six-hour-stale
    // episode decays to ~0.0125 and the equally-important fresh one (worst base rank, with
    // four spreaders widening the importance gap) climbs over it. Inverted to the semantic
    // clock the stale episode keeps ~0.997 of stored and the climb never happens. All
    // episodes ingest at T0, so recency is uniform and cannot cause the lift.
    let store = store();
    episode_with_stats(&store, "stale remark", stats(0.8, at(0), false), at(0), 0);
    episode_with_stats(&store, "fresh remark", stats(0.8, at(6), false), at(0), 1);
    for i in 0..4u32 {
        episode_with_stats(
            &store,
            &format!("spreader remark {i}"),
            stats(0.7 - 0.1 * f64::from(i), at(6), false),
            at(0),
            u128::from(i) + 2,
        );
    }

    let bundle = recall(
        &retriever(store, decay_on_split()),
        QueryClass::Temporal,
        Some(at(6)),
    )
    .await;

    let fresh = episode_rank(&bundle, "fresh remark").expect("fresh episode surfaced");
    let stale = episode_rank(&bundle, "stale remark").expect("stale episode surfaced");
    assert!(
        fresh < stale,
        "on the episodic clock six hours is six half-lives: the fresh episode climbs          (fresh #{fresh}, stale #{stale})"
    );
    assert!(
        episode_signals(&bundle, "stale remark").is_some_and(|s| s.contains(&Signal::Importance)),
        "the stale episode sank under an Importance contribution, it was not dropped"
    );
}

#[tokio::test]
async fn the_recency_re_rank_lifts_a_newly_ingested_episode() {
    // The recall (temporal) class weights recency HEAVY. The newer episode is created second
    // (worse base node-id rank) but ingested two hours later; importance and trust are
    // uniform, so the lift is purely the recency signal reading `ingested_at`.
    let store = store();
    episode_ingested_at(&store, "first captured remark", at(0), 0);
    episode_ingested_at(&store, "second captured remark", at(2), 1);
    // A middle-aged episode between them widens the recency-rank gap past the newer
    // episode's one-position dense deficit.
    episode_ingested_at(&store, "middle captured remark", at(1), 2);

    let bundle = recall(
        &retriever(store, RetrieverConfig::default()),
        QueryClass::Temporal,
        Some(at(3)),
    )
    .await;

    let newer = episode_rank(&bundle, "second captured remark").expect("newer surfaced");
    let older = episode_rank(&bundle, "first captured remark").expect("older surfaced");
    assert!(
        newer < older,
        "the later-ingested episode (worse base rank) climbs on recency (newer #{newer}, \
         older #{older})"
    );
    assert!(
        episode_signals(&bundle, "second captured remark")
            .is_some_and(|s| s.contains(&Signal::Recency)),
        "the surfaced episode carries a Recency contribution"
    );
    assert!(
        bundle.explanation.signals_run.contains(&Signal::Recency),
        "the recency signal is reported as run"
    );
}

#[tokio::test]
async fn a_uniform_set_collapses_and_reorders_nothing() {
    // The competition rank end to end: every fact shares one importance and one ingestion
    // instant, so with a clock supplied both re-ranks collapse to a single shared rank, add
    // the same constant to every candidate in fusion, and the order is IDENTICAL to the
    // clockless recall — equal values carry no signal, and no node-id bias leaks in.
    let store = store();
    let subj = subject(&store);
    for i in 0..5 {
        fact_with_stats(
            &store,
            &subj,
            &format!("uniform claim {i}"),
            stats(0.5, at(0), false),
            near(i),
        );
    }
    let retriever = retriever(store, RetrieverConfig::default());

    let without_clock = recall(&retriever, QueryClass::SingleHopFactual, None).await;
    let with_clock = recall(&retriever, QueryClass::SingleHopFactual, Some(at(6))).await;

    assert!(
        with_clock
            .explanation
            .signals_run
            .contains(&Signal::Importance),
        "the importance signal ran on the clocked recall"
    );
    assert_eq!(
        statement_order(&without_clock),
        statement_order(&with_clock),
        "a uniform set adds a constant to every candidate and reorders nothing"
    );
}

#[tokio::test]
async fn the_quote_class_gives_the_re_ranks_no_weight() {
    // The weight gate: the quote class zeroes importance and recency, so neither runs even
    // with a clock supplied and a lexically-surfaced candidate to order.
    let store = store();
    let subj = subject(&store);
    fact_with_stats(
        &store,
        &subj,
        "the recurring topic was settled",
        stats(0.9, at(0), false),
        near(0),
    );

    let bundle = recall(
        &retriever(store, RetrieverConfig::default()),
        QueryClass::Quote,
        Some(at(6)),
    )
    .await;

    for signal in [Signal::Importance, Signal::Recency] {
        assert!(
            !bundle.explanation.signals_run.contains(&signal),
            "{signal:?} must not run for the quote class"
        );
    }
}
