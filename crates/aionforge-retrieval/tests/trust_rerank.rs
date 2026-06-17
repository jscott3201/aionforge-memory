//! Integration tests for the trust re-rank signal in recall (M4.T05 PR-F, 03 §1, 06 §5).
//!
//! Trust is a *re-rank*, not a retrieval: it orders the candidates the search signals already
//! surfaced by their stored trust (facts by `Fact.stats.trust`, episodes by `Episode.stats.trust`),
//! best-first, and folds that ranking into reciprocal-rank fusion under [`Signal::Trust`]. These
//! tests pin: it runs and attributes (a surfaced fact carries a `Trust` contribution and the signal
//! is reported); it is decisive where it can be (a high-trust fact with the *worst* base rank still
//! climbs over a lower-trust one — a pure trust effect, since the base signals rank it last); and it
//! is gated (a class that gives trust no weight produces no `Trust` contribution).
//!
//! Hermetic: a fake embedder maps the query and the fact bodies to one fixed vector, so every fact
//! is equally dense-relevant and the base signals separate them only by node id (creation order).
//! Subject entities sit at a far vector and the query names none, so no entity resolves — the
//! associative graph/support signals stay idle and trust is the only thing that can reorder.

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

const T0: &str = "2026-01-01T00:00:00Z[UTC]";
const QUERY: &str = "the recurring topic";
const NEAR: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
const FAR: [f32; 4] = [0.0, 1.0, 0.0, 0.0];

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
        // Every input the recall embeds — the query and (at capture) the fact bodies — maps to the
        // one NEAR vector, so all facts are equally dense-relevant and only trust and node id order
        // them.
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

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime")
}

fn store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store.migrate(&ts(T0)).expect("migrate store");
    Arc::new(store)
}

fn stats(trust: f64) -> Stats {
    Stats {
        importance: 0.5,
        trust,
        last_access: ts(T0),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

fn identity(id: Id) -> Identity {
    Identity {
        id,
        ingested_at: ts(T0),
        namespace: Namespace::Global,
        expired_at: None,
    }
}

/// A subject entity at the FAR vector with a name the query never uses, so it never resolves as a
/// graph seed and the associative signals stay idle.
fn subject(store: &Store) -> (Id, NodeId) {
    let id = Id::generate();
    let ent = Entity {
        identity: identity(id),
        stats: stats(0.5),
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

/// Assert a NEAR-embedded fact with a given `trust`. The statement shares no token with the query,
/// so the lexical signal never separates the facts — only dense (tied) and trust do.
fn fact_with_trust(store: &Store, statement: &str, trust: f64) -> Id {
    let (subject_id, subject_node) = subject(store);
    let id = Id::generate();
    let f = Fact {
        identity: identity(id),
        stats: stats(trust),
        subject_id,
        predicate: "rel".to_string(),
        object: ObjectValue::Text(statement.to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: statement.to_string(),
        embedding: Some(Embedding::new(NEAR.to_vec()).expect("valid")),
        embedder_model: None,
        extraction: None,
        cooled_until: None,
    };
    let about = About {
        temporal: BiTemporal {
            valid_from: ts(T0),
            valid_to: None,
            ingested_at: ts(T0),
            expired_at: None,
        },
    };
    store
        .assert_fact(&f, subject_node, &about)
        .expect("assert fact");
    id
}

/// Insert a NEAR-embedded episode with a given `trust` and content; returns nothing.
fn episode_with_trust(store: &Store, content: &str, trust: f64, seed: u128) {
    let episode = Episode {
        identity: identity(Id::generate()),
        stats: stats(trust),
        content: content.to_string(),
        role: Role::User,
        captured_at: ts(T0),
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(&seed.to_le_bytes()),
        embedding: Some(Embedding::new(NEAR.to_vec()).expect("valid")),
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("insert episode");
}

fn episode_signals(bundle: &RecallBundle, content: &str) -> Option<Vec<Signal>> {
    bundle.structured.iter().find_map(|e| match e {
        StructuredEntry::Episode(ep) if ep.content == content => {
            Some(ep.contributions.iter().map(|c| c.signal).collect())
        }
        _ => None,
    })
}

fn episode_rank(bundle: &RecallBundle, content: &str) -> Option<usize> {
    bundle
        .structured
        .iter()
        .position(|e| matches!(e, StructuredEntry::Episode(ep) if ep.content == content))
}

/// Insert an episode with a given `trust` and content but NO embedding, so the dense signal cannot
/// reach it and only the lexical signal (and the trust re-rank) order it.
fn episode_no_embedding(store: &Store, content: &str, trust: f64, seed: u128) {
    let episode = Episode {
        identity: identity(Id::generate()),
        stats: stats(trust),
        content: content.to_string(),
        role: Role::User,
        captured_at: ts(T0),
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(&seed.to_le_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("insert episode");
}

fn retriever(store: Arc<Store>) -> HybridRetriever<FakeEmbedder> {
    HybridRetriever::new(
        store,
        FakeEmbedder::new(),
        RetrieverConfig {
            default_fanout: 50,
            support_expansion_depth: 1,
            ..RetrieverConfig::default()
        },
    )
}

async fn recall(r: &HybridRetriever<FakeEmbedder>, class: QueryClass) -> RecallBundle {
    r.recall(RecallQuery {
        text: QUERY.to_string(),
        principal: Principal::agent(Id::generate()),
        limit: 20,
        options: RecallOptions {
            mode_override: Some(class),
            fanout: 20,
            // Isolate the trust re-rank from the factual class's default dense floor
            // (router `FACTUAL_FLOOR`). These hermetic facts share one NEAR vector, and the
            // last-created climber can fall outside the dense ANN's returned candidate set —
            // so it is surfaced by the trust re-rank yet carries no dense score, exactly the
            // non-dense hit the floor drops by design. Floor rejection is covered by
            // min_relevance_floor.rs; here we pin the trust ordering itself.
            min_relevance: Some(0.0),
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

const LAST_LOW: &str = "low trust claim 4";
const HIGH: &str = "the high trust claim";

/// Five lower-trust facts created first (so they hold the better dense / node-id ranks), at
/// *distinct* descending trusts so the competition rank spreads them, then one high-trust fact
/// created last (so it holds the *worst* base rank). Only trust can lift it. `LAST_LOW` (the
/// lowest-trust, last-created peer) is the one the high-trust fact must climb over.
fn corpus() -> Arc<Store> {
    let store = store();
    for i in 0..5 {
        fact_with_trust(
            &store,
            &format!("low trust claim {i}"),
            0.50 - 0.10 * i as f64,
        );
    }
    fact_with_trust(&store, HIGH, 0.95);
    store
}

#[tokio::test]
async fn the_trust_re_rank_lifts_a_high_trust_fact_over_its_better_ranked_low_trust_peers() {
    // The high-trust fact is created last, so every base signal (dense and recency, both tied on
    // score) ranks it dead last by node id. The only signal that prefers it is trust. So if it
    // climbs above a low-trust fact that was created before it, that is a pure trust effect.
    let bundle = recall(&retriever(corpus()), QueryClass::SingleHopFactual).await;

    let high_rank = fact_rank(&bundle, HIGH).expect("high-trust fact surfaced");
    let last_low_rank = fact_rank(&bundle, LAST_LOW).expect("low-trust fact surfaced");
    assert!(
        high_rank < last_low_rank,
        "the high-trust fact (created last, worst base rank) climbs over a lower-trust peer purely \
         on trust (high #{high_rank}, low #{last_low_rank})",
    );

    // And it climbed because the trust signal ran and attributed to it.
    assert!(
        fact_signals(&bundle, HIGH).is_some_and(|s| s.contains(&Signal::Trust)),
        "the surfaced fact carries a Trust contribution",
    );
    assert!(
        bundle.explanation.signals_run.contains(&Signal::Trust),
        "the trust signal is reported as run",
    );
}

#[tokio::test]
async fn the_trust_re_rank_attributes_to_episodes_too() {
    // The episode side of the re-rank (the `is_fact == false` branch): a class that surfaces
    // episodes and weights trust gives a surfaced episode a Trust contribution, read from
    // `Episode.stats.trust`. The recall (temporal) class surfaces episodes and weights trust.
    let store = store();
    episode_with_trust(&store, "the recurring topic came up again", 0.95, 1);
    episode_with_trust(&store, "a passing unrelated remark", 0.10, 2);
    let bundle = recall(&retriever(store), QueryClass::Temporal).await;

    assert!(
        bundle.explanation.signals_run.contains(&Signal::Trust),
        "the trust signal runs for the recall class",
    );
    assert!(
        episode_signals(&bundle, "the recurring topic came up again")
            .is_some_and(|s| s.contains(&Signal::Trust)),
        "a surfaced episode carries a Trust contribution",
    );
}

#[tokio::test]
async fn the_trust_re_rank_lifts_a_high_trust_episode_over_its_better_ranked_peers() {
    // The episode analogue of the decisive fact test, exercising the `is_fact == false` branch that
    // reads `Episode.stats.trust`. The high-trust episode is created last, so dense ranks it dead
    // last by node id; only trust can lift it over a lower-trust peer.
    let store = store();
    for i in 0..5 {
        episode_with_trust(
            &store,
            &format!("low trust episode {i}"),
            0.50 - 0.10 * i as f64,
            i as u128,
        );
    }
    episode_with_trust(&store, "the high trust episode", 0.95, 99);
    let bundle = recall(&retriever(store), QueryClass::Temporal).await;

    let high =
        episode_rank(&bundle, "the high trust episode").expect("high-trust episode surfaced");
    let last_low =
        episode_rank(&bundle, "low trust episode 4").expect("low-trust episode surfaced");
    assert!(
        high < last_low,
        "the high-trust episode (worst base rank) climbs over a lower-trust peer on trust \
         (high #{high}, low #{last_low})",
    );
    assert!(
        episode_signals(&bundle, "the high trust episode")
            .is_some_and(|s| s.contains(&Signal::Trust)),
        "the episode carries a Trust contribution",
    );
}

const LONG_MATCH: &str =
    "the recurring topic with a great deal of additional padding and filler words here";

#[tokio::test]
async fn a_uniform_trust_set_is_not_reordered_by_the_trust_signal() {
    // Neutrality — the property the competition rank exists for. Two equal-trust episodes with NO
    // embedding are ordered by the lexical signal alone, which favors the shorter, denser match
    // (BM25 length normalization). The short match is created LAST (the worst node-id tie-break), so
    // a trust ranking that spread equal-trust candidates by node id would cancel the lexical lift and
    // sink it. The competition rank collapses equal trusts to one position, so trust adds a constant
    // and the lexical order stands: the short match keeps the top slot.
    let store = store();
    episode_no_embedding(&store, LONG_MATCH, 0.80, 1);
    episode_no_embedding(&store, QUERY, 0.80, 2);
    let bundle = recall(&retriever(store), QueryClass::Temporal).await;

    let short = episode_rank(&bundle, QUERY).expect("short match surfaced");
    let long = episode_rank(&bundle, LONG_MATCH).expect("long match surfaced");
    assert!(
        short < long,
        "uniform trust does not sink the lexically-preferred short match (short #{short}, long #{long})",
    );
}

#[tokio::test]
async fn the_trust_signal_is_off_for_the_quote_class() {
    // The quote class is lexical-only (trust OFF), so no candidate gains a Trust contribution and
    // the signal is not reported. The query shares a token with the statements here so the
    // lexical-only class still surfaces them.
    let store = store();
    fact_with_trust(&store, "the recurring topic, restated high", 0.95);
    fact_with_trust(&store, "the recurring topic, restated low", 0.10);
    let bundle = recall(&retriever(store), QueryClass::Quote).await;

    assert!(
        !bundle.explanation.signals_run.contains(&Signal::Trust),
        "the trust signal does not run for the quote class",
    );
    assert!(
        fact_signals(&bundle, "the recurring topic, restated high")
            .is_some_and(|s| !s.contains(&Signal::Trust)),
        "a surfaced fact carries no Trust contribution when trust is off",
    );
}
