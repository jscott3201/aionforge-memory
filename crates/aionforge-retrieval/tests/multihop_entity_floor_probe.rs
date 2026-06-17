//! M1 characterization (hermetic): would a plain dense floor amputate graph/support-recovered
//! gold on the MultiHop/Entity classes, and what would a "dense-OR-signal" hybrid admit?
//!
//! Factual/Temporal floor safely because their admitted hits are direct lexical+dense
//! (`graph_expansion=false`). MultiHop/Entity run `graph_expansion=true`, so they recover gold
//! via the Support (one incoming `SUPPORTS` hop) and Graph (Personalized PageRank) signals —
//! and that gold can be FAR in vector space (dense cosine ~0). The floor gate in `select()`
//! drops any candidate with `dense_similarity < floor` (absent-from-dense-map → 0.0), so a
//! factual-style 0.60 floor would amputate exactly that associative recall.
//!
//! This runner builds a hermetic evidence graph (the `support_expansion` pattern: a named
//! entity the query resolves, a NEAR root fact, FAR facts that `SUPPORTS`-chain off it, plus a
//! disconnected FAR off-topic fact), recalls the MultiHop/Entity classes at floor 0.0 vs 0.60,
//! and prints, per fact: dense similarity, contributing signals, and whether it survives each
//! floor. From that it reports two numbers that gate the hybrid design (M2):
//!
//! - **AMPUTATION**: facts recovered by Support/Graph with dense < floor that the 0.60 floor
//!   drops — the recall a "dense-OR-signal" hybrid would save.
//! - **LEAK surface**: facts a hybrid would admit via the non-dense branch (dense < floor but
//!   carrying a Support/Graph contribution), split into intended (on the resolved entity's
//!   evidence chain) vs incidental (anything else the graph signal dragged in).
//!
//! Two tests: a CI guard (`hybrid_admits_..._but_not_disconnected_offtopic`) pinning the
//! implemented dense-OR-signal hybrid — graph/support-recovered FAR gold survives an active
//! floor while a disconnected FAR off-topic hit does not — and an `#[ignore]` characterization
//! runner that prints the full per-fact amputation/leak table on demand.

// The output IS the deliverable: a human reads the printed tables.
#![allow(clippy::print_stdout)]

use std::collections::HashSet;
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
/// The live factual/temporal floor, applied here to the graph-expansion classes to show the
/// amputation it would cause if inherited.
const FLOOR: f64 = 0.60;

const NEAR: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
const FAR: [f32; 4] = [0.0, 1.0, 0.0, 0.0];
const FILLERS: usize = 5;

// The corpus statements, tagged by role so the report can label intended vs incidental.
const ROOT_FACT: &str = "acme is the primary subject"; // NEAR, the resolved entity's root
const EV1_FACT: &str = "first-hop evidence supporting acme"; // FAR, SUPPORTS root
const EV2_FACT: &str = "second-hop evidence behind the first"; // FAR, SUPPORTS ev1
const OFFTOPIC_FACT: &str = "disconnected unrelated detail"; // FAR, no edges to acme
const NOISE_FACT: &str = "near standalone chatter"; // NEAR, unrelated entity

/// The FAR facts that are genuinely on the resolved entity's evidence chain.
const INTENDED_FAR: [&str; 2] = [EV1_FACT, EV2_FACT];

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
        // The query embeds at NEAR (so it resolves the acme entity and the root); record
        // embeddings are stored explicitly below.
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
    vec: [f32; 4],
) -> Id {
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
        embedding: Some(Embedding::new(vec.to_vec()).expect("valid")),
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

/// Wire `Fact -SUPPORTS-> Fact` by domain id.
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

/// acme (NEAR) ← root (NEAR) ← ev1 (FAR) ← ev2 (FAR); a disconnected FAR off-topic fact; and
/// NEAR noise on a far entity. Fillers fill entity resolution so only acme seeds the roots.
fn corpus() -> Arc<Store> {
    let store = store();
    let (acme, acme_node) = entity(&store, QUERY, NEAR);
    let root = assert_fact(&store, &acme, acme_node, ROOT_FACT, NEAR);

    let (src, src_node) = entity(&store, "source", FAR);
    let ev1 = assert_fact(&store, &src, src_node, EV1_FACT, FAR);
    let ev2 = assert_fact(&store, &src, src_node, EV2_FACT, FAR);
    support(&store, &ev1, &root); // ev1 SUPPORTS root (one incoming hop from root)
    support(&store, &ev2, &ev1); // ev2 SUPPORTS ev1 (a second hop)

    let (off, off_node) = entity(&store, "elsewhere", FAR);
    assert_fact(&store, &off, off_node, OFFTOPIC_FACT, FAR); // no edges to acme

    let (noise, noise_node) = entity(&store, "noise", FAR);
    assert_fact(&store, &noise, noise_node, NOISE_FACT, NEAR);

    for n in 0..FILLERS {
        entity(&store, &format!("filler{n}"), NEAR);
    }
    store
}

fn retriever() -> HybridRetriever<FakeEmbedder> {
    HybridRetriever::new(
        corpus(),
        FakeEmbedder::new(),
        RetrieverConfig {
            default_fanout: 50,
            support_expansion_depth: 1,
            ..RetrieverConfig::default()
        },
    )
}

async fn recall(
    r: &HybridRetriever<FakeEmbedder>,
    class: QueryClass,
    floor: Option<f64>,
) -> RecallBundle {
    r.recall(RecallQuery {
        text: QUERY.to_string(),
        principal: Principal::agent(Id::generate()),
        limit: 20,
        options: RecallOptions {
            mode_override: Some(class),
            fanout: 20,
            min_relevance: floor,
            ..RecallOptions::default()
        },
    })
    .await
    .expect("recall")
}

/// (statement, dense_similarity, contributing signals) for every Fact in the bundle.
fn facts(bundle: &RecallBundle) -> Vec<(String, Option<f64>, Vec<Signal>)> {
    bundle
        .structured
        .iter()
        .filter_map(|e| match e {
            StructuredEntry::Fact(f) => Some((
                f.statement.clone(),
                e.dense_similarity(),
                f.contributions.iter().map(|c| c.signal).collect(),
            )),
            _ => None,
        })
        .collect()
}

fn graph_or_support(signals: &[Signal]) -> bool {
    signals.contains(&Signal::Support) || signals.contains(&Signal::Graph)
}

async fn report(class: QueryClass) {
    let r = retriever();
    let base = recall(&r, class, Some(0.0)).await;
    let floored_bundle = recall(&r, class, Some(FLOOR)).await;
    let floored: HashSet<String> = facts(&floored_bundle)
        .into_iter()
        .map(|(s, _, _)| s)
        .collect();

    println!("\n================ {class:?} — floor {FLOOR} amputation/leak probe ================");
    println!(
        "fact                                        dense   signals                      survives_{FLOOR}"
    );
    println!(
        "------------------------------------------------------------------------------------------------"
    );
    let mut amputated = Vec::new();
    let mut leak_intended = Vec::new();
    let mut leak_incidental = Vec::new();
    for (statement, dense, signals) in facts(&base) {
        let survives = floored.contains(&statement);
        let dense_str = dense.map_or("none".to_string(), |d| format!("{d:.3}"));
        let sigs: Vec<String> = signals.iter().map(|s| format!("{s:?}")).collect();
        println!(
            "{statement:<42}  {dense_str:<6}  {:<28}  {}",
            sigs.join(","),
            if survives { "yes" } else { "DROPPED" }
        );
        let low_dense = dense.unwrap_or(0.0) < FLOOR;
        if low_dense && graph_or_support(&signals) {
            // A hybrid (admit on dense>=floor OR a graph/support contribution) would admit this.
            if INTENDED_FAR.contains(&statement.as_str()) {
                leak_intended.push(statement.clone());
            } else {
                leak_incidental.push(statement.clone());
            }
            if !survives {
                amputated.push(statement.clone());
            }
        }
    }

    println!(
        "\nAMPUTATION (graph/support-recovered, dense<{FLOOR}, dropped by the plain floor): {} {:?}",
        amputated.len(),
        amputated
    );
    println!(
        "HYBRID would re-admit via the non-dense branch: intended(on acme's chain)={} {:?} | \
         incidental(leak)={} {:?}",
        leak_intended.len(),
        leak_intended,
        leak_incidental.len(),
        leak_incidental
    );
    println!(
        "  off-topic disconnected fact ({OFFTOPIC_FACT:?}) admitted via non-dense branch? {}",
        leak_incidental.iter().any(|s| s == OFFTOPIC_FACT)
    );

    // M3-stable invariants — properties of the graph STRUCTURE, true regardless of the floor
    // or the eventual hybrid (so they survive M3): the connected first-hop evidence is
    // Support-recovered (the recovery path the hybrid protects exists), and the disconnected
    // off-topic fact carries no Support/Graph contribution (so a dense-OR-signal hybrid cannot
    // leak it). The amputation itself is pre-hybrid and is only printed, not asserted.
    let base_facts = facts(&base);
    let signals_of = |needle: &str| {
        base_facts
            .iter()
            .find(|(s, _, _)| s == needle)
            .map(|(_, _, sig)| sig.clone())
    };
    let ev1 = signals_of(EV1_FACT).expect("first-hop evidence is in the floor-0.0 bundle");
    assert!(
        ev1.contains(&Signal::Support),
        "{class:?}: first-hop evidence is recovered via Support: {ev1:?}",
    );
    let off = signals_of(OFFTOPIC_FACT).expect("off-topic fact is in the floor-0.0 bundle");
    assert!(
        !graph_or_support(&off),
        "{class:?}: the disconnected off-topic fact carries no Support/Graph contribution \
         (clean leak surface): {off:?}",
    );
}

/// M3 regression guard (runs in CI): with a dense floor active, the dense-OR-signal hybrid
/// admits graph/support-recovered gold that is FAR in vector space, while still rejecting a
/// disconnected off-topic hit that is equally FAR but carries no graph/support contribution.
/// Exercised via a per-query floor so it holds regardless of the per-class profile default.
#[tokio::test]
async fn hybrid_admits_graph_support_gold_below_the_floor_but_not_disconnected_offtopic() {
    for class in [QueryClass::MultiHop, QueryClass::Entity] {
        let r = retriever();
        let present: HashSet<String> = facts(&recall(&r, class, Some(FLOOR)).await)
            .into_iter()
            .map(|(s, _, _)| s)
            .collect();
        assert!(
            present.contains(ROOT_FACT),
            "{class:?}: the NEAR root clears the dense floor",
        );
        assert!(
            present.contains(EV1_FACT),
            "{class:?}: Support-recovered FAR evidence survives the floor via the hybrid exemption",
        );
        assert!(
            present.contains(EV2_FACT),
            "{class:?}: Graph(PPR)-recovered FAR evidence (2-hop) survives via the hybrid exemption",
        );
        assert!(
            !present.contains(OFFTOPIC_FACT),
            "{class:?}: the disconnected FAR off-topic fact is still floored (no graph/support exemption)",
        );
    }
}

#[tokio::test]
#[ignore = "characterization runner: prints the full amputation/leak table; run with --ignored. Hermetic."]
async fn multihop_entity_floor_probe() {
    report(QueryClass::MultiHop).await;
    report(QueryClass::Entity).await;
}
