//! Integration tests for graph-expanded support scoring in recall (M3.T02 PR-B, 03 §1, §4).
//!
//! Support expansion is an *additive* signal, not a change to the dense pass. It takes the
//! query-entity fact roots (the §4 high-precision seed) and, for the graph-expansion classes
//! in Current mode, expands them one incoming `SUPPORTS` hop to recover the supporting
//! evidence a plain ANN pass leaves behind — vector-scored and composed natively with the
//! current-support set so nothing non-current leaks in, and emitted under [`Signal::Support`]
//! alongside the untouched dense pass. These tests pin the three acceptance criteria:
//! expansion recovers evidence the dense pass misses (the evidence gains a `Support`
//! contribution only when the depth knob is on); current precision does not regress (a near,
//! non-root fact keeps its `Dense` contribution whether or not expansion runs, and the
//! recovered evidence never outranks the root it supports); and the depth/fan-out is a
//! bounded, tunable knob (depth 0 disables it, an oversized depth clamps to the cap).
//!
//! Asserting on `Signal::Support` — not on the evidence merely being present — is
//! load-bearing: the evidence also surfaces via the undirected PageRank graph signal at
//! *both* depths (M3.T01), so a presence check could not tell expansion apart from PageRank.
//! `Support` is the signal only expansion produces.
//!
//! Hermetic: a fake embedder maps queries and records to small fixed vectors. Filler
//! entities at the query vector fill the entity resolution so only the real subject seeds
//! the roots, and near-query noise facts plus a tight fan-out keep the far evidence out of
//! the plain dense pass — so its recovery is attributable to support expansion.

use std::future::Future;
use std::sync::Arc;

use aionforge_domain::Retriever;
use aionforge_domain::authz::Principal;
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::edges::About;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::value::ObjectValue;
use aionforge_retrieval::{
    HybridRetriever, QueryClass, RecallBundle, RecallOptions, RecallQuery, RetrieverConfig, Signal,
    StructuredEntry,
};
use aionforge_store::{BoundQuery, NodeId, Store, StoreConfig, Value};

const T0: &str = "2026-01-01T00:00:00Z[UTC]";
const QUERY: &str = "acme";
/// A fact about the named entity, near the query: the expansion root.
const ROOT_FACT: &str = "acme is the primary subject";
/// The root's supporting evidence: far in vector space, no token overlap, reached only by
/// expanding the root one incoming SUPPORTS hop.
const EVIDENCE_FACT: &str = "far downstream supporting detail";
/// A near, non-root, non-evidence fact: semantically close to the query but about a far
/// (unresolved) entity and supporting nothing. The acceptance-#2 regression guard — its
/// `Dense` contribution must survive whether or not support expansion runs.
const NEIGHBOR_FACT: &str = "a nearby standalone claim";

const NEAR: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
const FAR: [f32; 4] = [0.0, 1.0, 0.0, 0.0];
/// Filler entities at the query vector, enough to fill the entity vector search so the far
/// evidence/noise entities never become expansion roots — only `acme` does. `acme` is
/// created before the fillers, so among the equidistant NEAR entities it holds the lowest
/// node id and is the one selene's `distance.then(node_id)`-ascending top-`ENTITY_ROOTS` cut
/// deterministically keeps when the fillers tie it on distance.
const FILLERS: usize = 5;
/// A tight, equal fan-out and bundle limit. `effective_fanout` floors the fan-out at the
/// limit, so both are kept small; the near noise facts then push the far evidence past the
/// plain dense pass, making its recovery attributable to expansion.
const WINDOW: usize = 5;

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
    let ent = Entity {
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
    let node = store.insert_entity(&ent).expect("insert entity");
    (id, node)
}

fn assert_fact(
    store: &Store,
    subject: &Id,
    subject_node: NodeId,
    statement: &str,
    embedding: [f32; 4],
) -> (Id, NodeId) {
    let id = Id::generate();
    let f = Fact {
        identity: identity(id),
        stats: stats(),
        subject_id: *subject,
        predicate: "rel".to_string(),
        object: ObjectValue::Text(statement.to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
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
        .assert_fact(&f, subject_node, &about)
        .expect("assert fact");
    (id, node)
}

/// Wire `Fact -SUPPORTS-> Fact` by domain id (`weight` is `NOT NULL`, bound as a parameter).
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

/// The provenance-shaped corpus: the named entity `acme` with a near root fact, a far
/// evidence fact that `SUPPORTS` it (about a far, unseeded entity), filler entities that
/// fill the entity resolution, and `WINDOW` near noise facts (about a far entity) that crowd
/// the plain dense pass so the far evidence falls past a tight fan-out.
fn support_corpus() -> Arc<Store> {
    let store = store();
    let (acme, acme_node) = entity(&store, QUERY, NEAR);
    let (root_id, _) = assert_fact(&store, &acme, acme_node, ROOT_FACT, NEAR);

    let (ev_subject, ev_subject_node) = entity(&store, "source", FAR);
    let (ev_id, _) = assert_fact(&store, &ev_subject, ev_subject_node, EVIDENCE_FACT, FAR);
    support(&store, &ev_id, &root_id);

    for n in 0..FILLERS {
        entity(&store, &format!("filler{n}"), NEAR);
    }
    let (noise, noise_node) = entity(&store, "noise", FAR);
    for n in 0..WINDOW {
        assert_fact(
            &store,
            &noise,
            noise_node,
            &format!("noise {n} unrelated chatter"),
            NEAR,
        );
    }
    store
}

/// A corpus for the acceptance-#2 regression: the `acme` root, its far evidence, and one
/// near, non-root [`NEIGHBOR_FACT`] (about a far, unresolved entity, supporting nothing). No
/// crowding noise, so a generous fan-out keeps every fact in the bundle and the neighbor's
/// contributions are readable at both depths — the point is the dense pass, not which facts
/// win a tight cut.
fn precision_corpus() -> Arc<Store> {
    let store = store();
    let (acme, acme_node) = entity(&store, QUERY, NEAR);
    let (root_id, _) = assert_fact(&store, &acme, acme_node, ROOT_FACT, NEAR);

    let (ev_subject, ev_subject_node) = entity(&store, "source", FAR);
    let (ev_id, _) = assert_fact(&store, &ev_subject, ev_subject_node, EVIDENCE_FACT, FAR);
    support(&store, &ev_id, &root_id);

    // Near fact, far (unresolved) subject entity, no SUPPORTS to the root — so it is neither
    // a root nor evidence, but is dense-relevant to the query.
    let (other, other_node) = entity(&store, "other", FAR);
    assert_fact(&store, &other, other_node, NEIGHBOR_FACT, NEAR);

    for n in 0..FILLERS {
        entity(&store, &format!("filler{n}"), NEAR);
    }
    store
}

fn retriever(store: Arc<Store>, depth: usize) -> HybridRetriever<FakeEmbedder> {
    HybridRetriever::new(
        store,
        FakeEmbedder::new(&[(QUERY, NEAR)]),
        RetrieverConfig {
            default_fanout: 50,
            support_expansion_depth: depth,
            ..RetrieverConfig::default()
        },
    )
}

async fn recall(r: &HybridRetriever<FakeEmbedder>, class: QueryClass) -> RecallBundle {
    recall_with_limit(r, class, WINDOW).await
}

/// Recall with an explicit bundle limit (and matching fan-out). A wide limit keeps every
/// fact in the bundle, so a fact's contributions are readable even when it is out-competed
/// for a tight top-k slot.
async fn recall_with_limit(
    r: &HybridRetriever<FakeEmbedder>,
    class: QueryClass,
    limit: usize,
) -> RecallBundle {
    r.recall(RecallQuery {
        text: QUERY.to_string(),
        principal: Principal::agent(Id::generate()),
        limit,
        options: RecallOptions {
            mode_override: Some(class),
            fanout: limit,
            ..RecallOptions::default()
        },
    })
    .await
    .expect("recall")
}

/// The signals that contributed to the fact entry with `statement`, if it is in the bundle.
fn fact_signals(bundle: &RecallBundle, statement: &str) -> Option<Vec<Signal>> {
    bundle.structured.iter().find_map(|e| match e {
        StructuredEntry::Fact(f) if f.statement == statement => {
            Some(f.contributions.iter().map(|c| c.signal).collect())
        }
        _ => None,
    })
}

fn has_fact(bundle: &RecallBundle, statement: &str) -> bool {
    bundle
        .structured
        .iter()
        .any(|e| matches!(e, StructuredEntry::Fact(f) if f.statement == statement))
}

/// The 0-based position of the fact with `statement` in the fused-score-ordered bundle, or
/// `None` if it is absent. Used to assert relative rank (a lower index ranks higher).
fn fact_rank(bundle: &RecallBundle, statement: &str) -> Option<usize> {
    bundle
        .structured
        .iter()
        .position(|e| matches!(e, StructuredEntry::Fact(f) if f.statement == statement))
}

// --- Tests -----------------------------------------------------------------------

#[tokio::test]
async fn support_expansion_recovers_evidence_the_dense_pass_misses() {
    // Depth 1 (the default): support expansion takes the acme root, expands it one incoming
    // SUPPORTS hop, and vector-scores the evidence — so the far evidence gains a `Support`
    // contribution. Asserting on `Support` (not mere presence) is load-bearing: the evidence
    // also surfaces via the undirected PageRank graph signal at *both* depths, so a presence
    // check could not separate expansion from PageRank. `Support` is expansion's alone.
    let expanded = recall(&retriever(support_corpus(), 1), QueryClass::MultiHop).await;
    let with_expansion = fact_signals(&expanded, EVIDENCE_FACT);
    assert!(
        with_expansion
            .as_ref()
            .is_some_and(|s| s.contains(&Signal::Support)),
        "support expansion gives the far evidence a Support contribution: {with_expansion:?}",
    );

    // Depth 0 (knob off): the support signal does not run, so the evidence carries no
    // `Support` contribution. (It may still surface via the PageRank graph signal — that is
    // M3.T01, not the support signal under test here.)
    let plain = recall(&retriever(support_corpus(), 0), QueryClass::MultiHop).await;
    assert!(
        fact_signals(&plain, EVIDENCE_FACT).is_none_or(|s| !s.contains(&Signal::Support)),
        "with the knob off there is no Support contribution for the evidence",
    );
}

#[tokio::test]
async fn the_dense_pass_keeps_a_near_non_root_fact_under_expansion() {
    // Acceptance #2 — current precision stays full. Support expansion is ADDITIVE: it emits a
    // Support signal over a query entity's evidence; it must NOT narrow the dense pass. So a
    // near, non-root, non-evidence fact keeps its Dense contribution whether the knob is off
    // or on. A wide limit keeps every fact in the bundle so the neighbor's contributions are
    // readable at both depths.
    let off = recall_with_limit(&retriever(precision_corpus(), 0), QueryClass::MultiHop, 20).await;
    assert!(
        fact_signals(&off, NEIGHBOR_FACT).is_some_and(|s| s.contains(&Signal::Dense)),
        "the near non-root fact has a Dense contribution with the knob off",
    );

    let on = recall_with_limit(&retriever(precision_corpus(), 1), QueryClass::MultiHop, 20).await;
    assert!(
        fact_signals(&on, NEIGHBOR_FACT).is_some_and(|s| s.contains(&Signal::Dense)),
        "support expansion does not strip the near non-root fact's Dense contribution",
    );

    // And expansion still did its additive job: the evidence gains a Support contribution it
    // lacks with the knob off — recovered without disturbing the neighbor's dense ranking.
    assert!(
        fact_signals(&on, EVIDENCE_FACT).is_some_and(|s| s.contains(&Signal::Support)),
        "the evidence is recovered via the Support signal",
    );

    // Rank stability (finding #6): recovered evidence must not outrank the precision root it
    // supports.
    let root_rank = fact_rank(&on, ROOT_FACT).expect("root present");
    let evidence_rank = fact_rank(&on, EVIDENCE_FACT).expect("evidence recovered");
    assert!(
        root_rank < evidence_rank,
        "the root outranks its recovered evidence (root #{root_rank}, evidence #{evidence_rank})",
    );
}

#[tokio::test]
async fn current_precision_does_not_regress_under_expansion() {
    // The root fact about the named entity is the precision target. Expansion preserves the
    // roots, so it still surfaces with expansion on — and it surfaces for the single-hop
    // class too, where expansion is off and the seed-intersection precision path runs.
    let multi = recall(&retriever(support_corpus(), 1), QueryClass::MultiHop).await;
    assert!(
        has_fact(&multi, ROOT_FACT),
        "the precision root fact survives support expansion",
    );

    let single = recall(
        &retriever(support_corpus(), 1),
        QueryClass::SingleHopFactual,
    )
    .await;
    assert!(
        has_fact(&single, ROOT_FACT),
        "the precision root fact still surfaces for the single-hop class",
    );
}

#[tokio::test]
async fn single_hop_class_runs_no_support_expansion() {
    // The single-hop factual class turns graph expansion off, so neither the support signal
    // nor the PageRank signal runs — the evidence, reachable only by expanding the root, gets
    // no `Support` contribution and never surfaces.
    let bundle = recall(
        &retriever(support_corpus(), 1),
        QueryClass::SingleHopFactual,
    )
    .await;
    assert!(
        fact_signals(&bundle, EVIDENCE_FACT).is_none_or(|s| !s.contains(&Signal::Support)),
        "the support signal does not run for the single-hop class",
    );
    assert!(
        !has_fact(&bundle, EVIDENCE_FACT),
        "no support expansion for the single-hop class, so the evidence stays hidden",
    );
}

#[tokio::test]
async fn an_oversized_depth_is_clamped_to_the_cap() {
    // The depth knob is bounded: a depth far above the cap behaves exactly like the single
    // hop v1 supports — the evidence still gains its `Support` contribution, no runaway.
    let bundle = recall(&retriever(support_corpus(), 99), QueryClass::MultiHop).await;
    assert!(
        fact_signals(&bundle, EVIDENCE_FACT).is_some_and(|s| s.contains(&Signal::Support)),
        "an oversized depth clamps to the cap and still expands one hop",
    );
}

#[tokio::test]
async fn support_expansion_skips_gracefully_on_an_embedder_outage() {
    // With the embedder down there is no query vector, so the entity roots cannot be
    // resolved and support expansion is skipped — recall still returns, degraded, with the
    // outage flagged.
    let r = HybridRetriever::new(
        support_corpus(),
        FakeEmbedder::down(),
        RetrieverConfig {
            default_fanout: 50,
            support_expansion_depth: 1,
            ..RetrieverConfig::default()
        },
    );
    let bundle = recall(&r, QueryClass::MultiHop).await;
    assert!(
        !bundle.explanation.embedder_available,
        "the embedder is reported down",
    );
    assert!(
        fact_signals(&bundle, EVIDENCE_FACT).is_none_or(|s| !s.contains(&Signal::Support)),
        "support expansion is skipped without a query vector to resolve roots",
    );
}
