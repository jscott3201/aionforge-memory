//! Integration tests for the associative graph signal (M3.T01 PR-B, 03 §1, §3).
//!
//! Graph expansion seeds Personalized PageRank on the entities a query names and spreads
//! mass across the associative graph — `Episode -MENTIONS-> Entity`, `Fact -ABOUT->
//! Entity`, `Fact|Episode -SUPPORTS-> Fact` — so a record reachable only through that
//! structure (a fact two hops away over a `SUPPORTS` chain, an episode that mentions the
//! entity) surfaces even when it sits far from the query in vector space and shares no
//! token with it. These tests pin: graph expansion surfaces such records (proven by the
//! record's only fusion contribution being the graph signal); the router gates it
//! (suppressed for the single-hop class) and it is skipped when no entity seed resolves;
//! Current mode never lets a graph reach leak a non-current fact the support provider
//! excludes; and the signal survives an embedder outage because seeds also resolve lexically.
//!
//! Hermetic: a fake embedder maps queries and records to small fixed vectors, and a tight
//! fan-out plus near-query distractors push the graph-only records past the dense and
//! lexical signals, so what surfaces them is the graph signal, not vector or text recall.

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
    StructuredEntry, TemporalMode,
};
use aionforge_store::{BoundQuery, NodeId, Store, StoreConfig, Value};

const T0: &str = "2026-01-01T00:00:00Z[UTC]";
const QUERY: &str = "acme";
/// Direct fact about the seed entity — near the query, lexically overlapping.
const DIRECT_FACT: &str = "acme is the primary subject";
/// A fact two hops away over a `SUPPORTS` chain — far from the query, no token overlap,
/// so only the graph signal reaches it.
const CHAINED_FACT: &str = "beta downstream detail";
/// A fact about the seed entity that an outgoing `CONTRADICTS` quarantines: still
/// `Active`, but dropped from `current_support_facts`.
const CONTESTED_FACT: &str = "contested claim";
/// An episode that mentions the seed entity but is far from the query and shares no token.
const MENTIONED_EPISODE: &str = "an offhand remark";

const NEAR: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
const FAR: [f32; 4] = [0.0, 1.0, 0.0, 0.0];

/// Filler entities at the query vector, enough to fill the entity vector search (top
/// `ENTITY_ROOTS` = 5 in the retriever) so the far fact-bearing entities never seed the
/// walk — only the lexically-matched `acme` does.
const FILLERS: usize = 5;

// --- Fake embedder ---------------------------------------------------------------

#[derive(Clone)]
struct FakeEmbedder {
    model: EmbedderModel,
    query_vectors: Vec<(String, [f32; 4])>,
    down: bool,
}

impl FakeEmbedder {
    fn new(query_vectors: &[(&str, [f32; 4])]) -> Self {
        Self {
            model: EmbedderModel {
                family: "fake".to_string(),
                version: "1".to_string(),
                dimension: 4,
            },
            query_vectors: query_vectors
                .iter()
                .map(|(q, v)| ((*q).to_string(), *v))
                .collect(),
            down: false,
        }
    }

    fn down() -> Self {
        let mut e = Self::new(&[]);
        e.down = true;
        e
    }
}

#[derive(Debug)]
struct FakeEmbedError;

impl std::fmt::Display for FakeEmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("fake embedder is down")
    }
}

impl std::error::Error for FakeEmbedError {}

impl Embedder for FakeEmbedder {
    type Error = FakeEmbedError;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let result = if self.down {
            Err(FakeEmbedError)
        } else {
            Ok(inputs
                .iter()
                .map(|input| {
                    let v = self
                        .query_vectors
                        .iter()
                        .find(|(q, _)| q == input)
                        .map(|(_, v)| v.to_vec())
                        .unwrap_or_else(|| NEAR.to_vec());
                    Embedding::new(v).expect("valid fake embedding")
                })
                .collect())
        };
        async move { result }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

// --- Fixtures --------------------------------------------------------------------

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime")
}

fn zdt() -> Value {
    Value::ZonedDateTime(Box::new(ts(T0)))
}

fn store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store.migrate(&ts(T0)).expect("migrate store");
    Arc::new(store)
}

fn stats() -> Stats {
    Stats {
        importance: 0.5,
        trust: 0.8,
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

fn entity(store: &Store, name: &str, embedding: [f32; 4]) -> (Id, NodeId) {
    let id = Id::generate();
    let entity = Entity {
        identity: identity(id),
        stats: stats(),
        canonical_name: name.to_string(),
        entity_type: "Concept".to_string(),
        aliases: vec![],
        description: None,
        embedding: Some(Embedding::new(embedding.to_vec()).expect("valid")),
        embedder_model: None,
        attributes: None,
    };
    let node = store.insert_entity(&entity).expect("insert entity");
    (id, node)
}

fn assert_fact(
    store: &Store,
    subject: &Id,
    subject_node: NodeId,
    statement: &str,
    embedding: [f32; 4],
    status: FactStatus,
) -> (Id, NodeId) {
    let id = Id::generate();
    let fact = Fact {
        identity: identity(id),
        stats: stats(),
        subject_id: *subject,
        predicate: "rel".to_string(),
        object: ObjectValue::Text(statement.to_string()),
        confidence: 0.9,
        status,
        statement: statement.to_string(),
        embedding: Some(Embedding::new(embedding.to_vec()).expect("valid")),
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
    let node = store
        .assert_fact(&fact, subject_node, &about)
        .expect("assert fact");
    (id, node)
}

fn episode(store: &Store, content: &str, embedding: [f32; 4]) -> Id {
    let id = Id::generate();
    let episode = Episode {
        identity: identity(id),
        stats: stats(),
        content: content.to_string(),
        role: Role::User,
        captured_at: ts(T0),
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: Some(Embedding::new(embedding.to_vec()).expect("valid")),
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("insert episode");
    id
}

/// Wire `Episode -MENTIONS-> Entity` by domain id (no typed writer exists yet; this is a
/// real commit through the parameter-bound write path). Both `NOT NULL` timestamps bound.
fn mention(store: &Store, episode_id: &Id, entity_id: &Id) {
    let q = BoundQuery::new(
        "MATCH (e:Episode {id: $from}), (n:Entity {id: $to}) \
         INSERT (e)-[:MENTIONS {valid_from: $ts, ingested_at: $ts}]->(n)",
    )
    .bind_uuid("from", episode_id)
    .unwrap()
    .bind_uuid("to", entity_id)
    .unwrap()
    .bind("ts", zdt())
    .unwrap();
    store.execute(&q).expect("insert MENTIONS");
}

/// Wire `Fact -SUPPORTS-> Fact` by domain id (`weight` is `NOT NULL` on the type, bound
/// as a parameter like every other GQL value).
fn support(store: &Store, from: &Id, to: &Id) {
    let q = BoundQuery::new(
        "MATCH (a:Fact {id: $from}), (b:Fact {id: $to}) \
         INSERT (a)-[:SUPPORTS {weight: $weight}]->(b)",
    )
    .bind_uuid("from", from)
    .unwrap()
    .bind_uuid("to", to)
    .unwrap()
    .bind("weight", Value::Float(1.0))
    .unwrap();
    store.execute(&q).expect("insert SUPPORTS");
}

/// Wire `Fact -CONTRADICTS-> Fact` by domain id. The outgoing source is the quarantined
/// fact `current_support_facts` excludes, while its node `status` stays `Active`.
fn contradict(store: &Store, from: &Id, to: &Id) {
    let q = BoundQuery::new(
        "MATCH (a:Fact {id: $from}), (b:Fact {id: $to}) \
         INSERT (a)-[:CONTRADICTS {valid_from: $ts, ingested_at: $ts, detected_by: $by}]->(b)",
    )
    .bind_uuid("from", from)
    .unwrap()
    .bind_uuid("to", to)
    .unwrap()
    .bind("ts", zdt())
    .unwrap()
    .bind_str("by", "contradiction-detector")
    .unwrap();
    store.execute(&q).expect("insert CONTRADICTS");
}

/// The seed entity and the facts a graph test needs to reference after building the graph.
struct Seeded {
    store: Arc<Store>,
    /// The seed entity `acme`: domain id and node id.
    acme: Id,
    acme_node: NodeId,
    /// The direct fact about the seed entity (a CONTRADICTS target in the scoping test).
    direct_fact: Id,
}

/// The associative graph the fact tests share. The seed entity `acme` has a direct fact;
/// the chained fact is about a far `beta` entity, reached only through a `SUPPORTS` hop
/// from the direct fact. To make the chained fact graph-only without polluting the seeds:
///
/// - **Filler entities** (no facts) sit at the query vector. `resolve_seed_entities` takes
///   the top entities by vector, so the fillers — not the far fact-bearing entities —
///   fill that search; only `acme` (also matched lexically) actually seeds the walk.
/// - **Near distractor facts** hang off a far, unseeded, disconnected `noise` entity. They
///   crowd the dense fact ranking so a tight fan-out drops the far chained fact, but
///   PageRank never reaches them (their entity is neither a seed nor in `acme`'s component).
///
/// So in Current mode the chained fact surfaces only via the graph signal.
fn supports_chain_store() -> Seeded {
    let store = store();
    let (acme, acme_node) = entity(&store, QUERY, NEAR);
    let (direct_fact, _) = assert_fact(
        &store,
        &acme,
        acme_node,
        DIRECT_FACT,
        NEAR,
        FactStatus::Active,
    );

    let (beta, beta_node) = entity(&store, "beta", FAR);
    let (chained, _) = assert_fact(
        &store,
        &beta,
        beta_node,
        CHAINED_FACT,
        FAR,
        FactStatus::Active,
    );
    support(&store, &direct_fact, &chained);

    for n in 0..FILLERS {
        entity(&store, &format!("filler{n}"), NEAR);
    }
    let (noise, noise_node) = entity(&store, "noise", FAR);
    for n in 0..WINDOW {
        assert_fact(
            &store,
            &noise,
            noise_node,
            &format!("noise {n} crowd detail"),
            NEAR,
            FactStatus::Active,
        );
    }
    Seeded {
        store,
        acme,
        acme_node,
        direct_fact,
    }
}

fn retriever(store: Arc<Store>, embedder: FakeEmbedder) -> HybridRetriever<FakeEmbedder> {
    HybridRetriever::new(store, embedder, RetrieverConfig::default())
}

fn embedder_up() -> FakeEmbedder {
    FakeEmbedder::new(&[(QUERY, NEAR)])
}

/// A small recall window. `effective_fanout` floors the per-signal fan-out at the bundle
/// `limit`, so both are kept tight (and equal) — that is what lets the near-query
/// distractors push a far, graph-only record past the dense ranking. A wider window would
/// sweep the whole small fixture graph and hide which signal did the surfacing.
const WINDOW: usize = 5;

async fn recall(
    r: &HybridRetriever<FakeEmbedder>,
    class: QueryClass,
    temporal: TemporalMode,
) -> RecallBundle {
    r.recall(RecallQuery {
        text: QUERY.to_string(),
        principal: Principal::agent(Id::generate()),
        limit: WINDOW,
        options: RecallOptions {
            mode_override: Some(class),
            temporal,
            fanout: WINDOW,
            ..RecallOptions::default()
        },
    })
    .await
    .expect("recall")
}

fn has_fact(bundle: &RecallBundle, statement: &str) -> bool {
    bundle
        .structured
        .iter()
        .any(|e| matches!(e, StructuredEntry::Fact(f) if f.statement == statement))
}

fn has_episode(bundle: &RecallBundle, content: &str) -> bool {
    bundle
        .structured
        .iter()
        .any(|e| matches!(e, StructuredEntry::Episode(ep) if ep.content == content))
}

/// The signals that contributed to the fact entry with `statement`, in fusion's canonical
/// order, or `None` if no such fact is in the bundle. Used to prove which signal surfaced a
/// record, not merely that it is present.
fn fact_signals(bundle: &RecallBundle, statement: &str) -> Option<Vec<Signal>> {
    bundle.structured.iter().find_map(|e| match e {
        StructuredEntry::Fact(f) if f.statement == statement => {
            Some(f.contributions.iter().map(|c| c.signal).collect())
        }
        _ => None,
    })
}

// --- Tests -----------------------------------------------------------------------

#[tokio::test]
async fn graph_expansion_surfaces_a_supports_chained_fact() {
    // The query names `acme`; the chained fact is two hops away (acme <- direct -SUPPORTS->
    // chained), far in vector space, with no token overlap. The WINDOW near noise facts fill
    // the dense top-k (fan-out is WINDOW), so the far chained fact falls outside both the
    // dense and lexical rankings — only graph expansion reaches it.
    let seeded = supports_chain_store();
    let r = retriever(seeded.store, embedder_up());

    let bundle = recall(&r, QueryClass::MultiHop, TemporalMode::Current).await;

    // Presence, and — decisively — no *search* signal other than graph reached the chained
    // fact. Dense never reaches it (far, past the fan-out) and lexical never reaches it (no
    // shared token), so graph expansion is what surfaced it, not a wide fan-out sweeping the
    // small fixture or a node-id tie-break. Trust re-ranks whatever the search signals surface,
    // so it contributes here too (M4.T05); that is expected and is not a surfacing signal.
    let chained_signals = fact_signals(&bundle, CHAINED_FACT).expect("chained fact present");
    assert!(
        chained_signals.contains(&Signal::Graph),
        "graph expansion surfaced the chained fact: {chained_signals:?}",
    );
    assert!(
        !chained_signals.contains(&Signal::Dense)
            && !chained_signals.contains(&Signal::Lexical)
            && !chained_signals.contains(&Signal::Support),
        "no other search signal (dense, lexical, support) reached the chained fact: {chained_signals:?}",
    );
    assert!(
        bundle.explanation.signals_run.contains(&Signal::Graph),
        "the graph signal ran and is recorded in the explanation",
    );

    // The whole path stays deterministic with the graph signal in the mix.
    let again = recall(&r, QueryClass::MultiHop, TemporalMode::Current).await;
    assert_eq!(
        bundle.rendered, again.rendered,
        "recall with graph expansion is byte-identical across calls",
    );
}

#[tokio::test]
async fn single_hop_class_suppresses_graph_expansion() {
    // The same graph under the single-hop factual class: the router turns graph expansion
    // off, so the chained fact — reachable only through the graph — never surfaces, and the
    // graph signal does not run.
    let seeded = supports_chain_store();
    let r = retriever(seeded.store, embedder_up());

    let bundle = recall(&r, QueryClass::SingleHopFactual, TemporalMode::Current).await;

    assert!(
        !has_fact(&bundle, CHAINED_FACT),
        "no graph expansion for the single-hop class, so the chained fact stays hidden",
    );
    assert!(
        !bundle.explanation.signals_run.contains(&Signal::Graph),
        "the graph signal is suppressed for the single-hop class",
    );
}

#[tokio::test]
async fn graph_expansion_surfaces_a_mentioned_episode() {
    // An episode that mentions the seed entity but is far from the query and shares no
    // token with it. Near-query distractor episodes plus a tight fan-out keep it out of the
    // dense and lexical episode rankings, so only graph expansion (MENTIONS) reaches it.
    let store = store();
    let (acme, _) = entity(&store, QUERY, NEAR);
    let ep = episode(&store, MENTIONED_EPISODE, FAR);
    mention(&store, &ep, &acme);
    // At least `WINDOW` near-query distractors so the far mentioned episode falls past the
    // dense top-k; only graph expansion can then surface it.
    for n in 0..WINDOW {
        episode(&store, &format!("chatter {n} about other things"), NEAR);
    }
    let r = retriever(store, embedder_up());

    let expanded = recall(&r, QueryClass::MultiHop, TemporalMode::Current).await;
    assert!(
        has_episode(&expanded, MENTIONED_EPISODE),
        "graph expansion surfaces the episode that mentions the seed entity",
    );

    let suppressed = recall(&r, QueryClass::SingleHopFactual, TemporalMode::Current).await;
    assert!(
        !has_episode(&suppressed, MENTIONED_EPISODE),
        "with graph expansion off, the mentioned episode stays hidden",
    );
}

#[tokio::test]
async fn current_mode_scoping_excludes_a_contradicted_but_active_fact() {
    // A fact about the seed entity that an outgoing CONTRADICTS quarantines: `status`
    // stays Active, but `current_support_facts` drops it. The lexical and dense fact
    // searches are bounded to the support set already; the graph reach is not, so without
    // the retriever's Current-mode intersection this fact would leak (its Active status is
    // all `fact_passes_temporal` checks in Current mode).
    let seeded = supports_chain_store();
    let (contested, _) = assert_fact(
        &seeded.store,
        &seeded.acme,
        seeded.acme_node,
        CONTESTED_FACT,
        FAR,
        FactStatus::Active,
    );
    // The contested fact contradicts the direct fact (its incumbent); the outgoing
    // CONTRADICTS source — the contested fact — is the one the support provider quarantines.
    contradict(&seeded.store, &contested, &seeded.direct_fact);
    let r = retriever(seeded.store, embedder_up());

    // Current mode: the contested fact is reached by the graph (it is about the seed) but
    // intersected out, while the genuinely current chained fact still surfaces.
    let current = recall(&r, QueryClass::MultiHop, TemporalMode::Current).await;
    assert!(
        !has_fact(&current, CONTESTED_FACT),
        "a contradicted-but-active fact is scoped out of a Current-mode graph reach",
    );
    assert!(
        has_fact(&current, CHAINED_FACT),
        "the genuinely current chained fact still surfaces in Current mode",
    );

    // History mode applies no current scoping, so the same graph reach now includes it —
    // proving it was the Current intersection, not unreachability, that excluded it.
    let history = recall(&r, QueryClass::MultiHop, TemporalMode::History).await;
    assert!(
        has_fact(&history, CONTESTED_FACT),
        "History mode keeps the contradicted fact the graph reaches",
    );
}

#[tokio::test]
async fn graph_expansion_survives_an_embedder_outage() {
    // With the embedder down there is no query vector, so the dense signal drops out and
    // entity seeds cannot be resolved by vector. The entity text index still resolves the
    // named entity by canonical name, so graph expansion runs and the supports-chained
    // fact — which only the graph reaches — still surfaces.
    let seeded = supports_chain_store();
    let r = retriever(seeded.store, FakeEmbedder::down());

    let bundle = recall(&r, QueryClass::Entity, TemporalMode::Current).await;

    assert!(
        !bundle.explanation.embedder_available,
        "the embedder is reported down",
    );
    assert!(
        bundle.explanation.signals_run.contains(&Signal::Graph),
        "graph expansion runs on lexically-resolved seeds even with the embedder down",
    );
    assert!(
        has_fact(&bundle, CHAINED_FACT),
        "the supports-chained fact surfaces through lexically-seeded graph expansion",
    );
}

#[tokio::test]
async fn graph_signal_is_skipped_when_no_entity_seed_resolves() {
    // An empty store: neither the entity text index nor the entity vector search resolves
    // anything, so `resolve_seed_entities` returns `None` and the retriever short-circuits
    // the graph signal rather than running an unseeded (global) PageRank. The router would
    // otherwise enable graph expansion for the multi-hop class, so this exercises the
    // seed-side gate, not the class gate.
    let r = retriever(store(), embedder_up());

    let bundle = recall(&r, QueryClass::MultiHop, TemporalMode::Current).await;

    assert!(
        !bundle.explanation.signals_run.contains(&Signal::Graph),
        "with no resolvable entity seed the graph signal does not run: {:?}",
        bundle.explanation.signals_run,
    );
}
