//! End-to-end tests for the high-precision default path (M2.T08, 03 §4).
//!
//! The path derives a graph candidate seed from the entities a query names, composes it
//! with the current-support set via native set algebra, and exact-vector-reranks the
//! bounded result. The headline scenario is the precision win the spec claims: a current
//! fact about the named entity surfaces even though it sits far from the query in vector
//! space and a plain ANN pass — crowded out by nearer facts about *other* entities —
//! would rank it past the fan-out.
//!
//! Hermetic: a fake embedder maps queries and records to small fixed vectors, so ranking
//! is decided by the seeding and set algebra, not by vector noise.

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
use aionforge_domain::nodes::core::{BlockKind, CoreBlock};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::semantic::{Entity, Fact, FactStatus};
use aionforge_domain::time::{BiTemporal, Timestamp};
use aionforge_domain::value::ObjectValue;
use aionforge_retrieval::{
    HybridRetriever, RecallBundle, RecallOptions, RecallQuery, RetrieverConfig, StructuredEntry,
    TemporalMode,
};
use aionforge_store::{NodeId, Store, StoreConfig};

const T0: &str = "2026-01-01T00:00:00Z[UTC]";

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

    fn down(query_vectors: &[(&str, [f32; 4])]) -> Self {
        let mut e = Self::new(query_vectors);
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
                        .unwrap_or_else(|| vec![1.0, 0.0, 0.0, 0.0]);
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

fn entity(store: &Store, name: &str, embedding: [f32; 4]) -> (Id, NodeId) {
    let id = Id::generate();
    let entity = Entity {
        identity: Identity {
            id,
            ingested_at: ts(T0),
            namespace: Namespace::Global,
            expired_at: None,
        },
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

/// An entity with no embedding — absent from the vector index, so a query never resolves
/// it. Used to force the no-seed dense fallback while the embedder is up.
fn entity_unembedded(store: &Store, name: &str) -> (Id, NodeId) {
    let id = Id::generate();
    let entity = Entity {
        identity: Identity {
            id,
            ingested_at: ts(T0),
            namespace: Namespace::Global,
            expired_at: None,
        },
        stats: stats(),
        canonical_name: name.to_string(),
        entity_type: "Concept".to_string(),
        aliases: vec![],
        description: None,
        embedding: None,
        embedder_model: None,
        attributes: None,
    };
    let node = store.insert_entity(&entity).expect("insert entity");
    (id, node)
}

fn assert_fact(store: &Store, subject: &Id, subject_node: NodeId, statement: &str, emb: [f32; 4]) {
    let fact = Fact {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts(T0),
            namespace: Namespace::Global,
            expired_at: None,
        },
        stats: stats(),
        subject_id: *subject,
        predicate: "based_in".to_string(),
        object: ObjectValue::Text(statement.to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: statement.to_string(),
        embedding: Some(Embedding::new(emb.to_vec()).expect("valid")),
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
        .assert_fact(&fact, subject_node, &about)
        .expect("assert fact");
}

/// The relocation-style fixture for the precision win:
/// - `acme` is the named entity, nearest the query, with ONE far-embedded current fact.
/// - five filler entities sit between acme and `other` in vector space, with no facts, so
///   the top-5 entity resolution is {acme + fillers} and `other` is never resolved.
/// - `other` is far from the query and owns several near-embedded current facts that a
///   plain ANN pass ranks above acme's fact.
///
/// The query vector is [1,0,0,0]; acme=[1,0,0,0], fillers=[0.8,0.6,0,0], other=[0,1,0,0].
fn precision_store() -> Arc<Store> {
    let store = store();

    let (acme, acme_node) = entity(&store, "acme", [1.0, 0.0, 0.0, 0.0]);
    // The target: far from the query in vector space, no lexical overlap with it.
    assert_fact(
        &store,
        &acme,
        acme_node,
        "alpha widget",
        [0.0, 1.0, 0.0, 0.0],
    );

    for n in 0..5 {
        entity(&store, &format!("filler{n}"), [0.8, 0.6, 0.0, 0.0]);
    }

    let (other, other_node) = entity(&store, "other", [0.0, 1.0, 0.0, 0.0]);
    for n in 0..5 {
        assert_fact(
            &store,
            &other,
            other_node,
            &format!("beta widget {n}"),
            [1.0, 0.0, 0.0, 0.0],
        );
    }
    store
}

fn retriever(store: Arc<Store>, embedder: FakeEmbedder) -> HybridRetriever<FakeEmbedder> {
    HybridRetriever::new(store, embedder, RetrieverConfig::default())
}

#[allow(clippy::too_many_arguments)]
async fn recall(
    r: &HybridRetriever<FakeEmbedder>,
    text: &str,
    temporal: TemporalMode,
    sensitive: bool,
) -> RecallBundle {
    r.recall(RecallQuery {
        text: text.to_string(),
        principal: Principal::agent(Id::generate()),
        limit: 3,
        options: RecallOptions {
            temporal,
            sensitive,
            // A tight fan-out so the far target fact would fall outside a plain ANN pass.
            fanout: 3,
            ..RecallOptions::default()
        },
    })
    .await
    .expect("recall")
}

fn has_fact(bundle: &RecallBundle, statement: &str) -> bool {
    bundle.structured.iter().any(|e| match e {
        StructuredEntry::Fact(f) => f.statement == statement,
        _ => false,
    })
}

// --- Tests -----------------------------------------------------------------------

#[tokio::test]
async fn high_precision_path_surfaces_a_current_fact_a_plain_ann_pass_misses() {
    // The query names acme (vector [1,0,0,0] resolves the acme entity), but acme's only
    // current fact sits far away at [0,1,0,0]. The five near distractor facts about
    // `other` (at [1,0,0,0]) would crowd it past the fan-out of 3 in a plain pass.
    let r = retriever(
        precision_store(),
        FakeEmbedder::new(&[("acme", [1.0, 0.0, 0.0, 0.0])]),
    );
    let bundle = recall(&r, "acme", TemporalMode::Current, false).await;

    assert!(
        has_fact(&bundle, "alpha widget"),
        "the seeded current fact about acme surfaces: {}",
        bundle.rendered,
    );
    // None of `other`'s distractor facts are pulled in by the dense signal: the seed
    // narrowed the dense candidate set to acme's facts (other was outside the top-5
    // entity resolution), so they have no dense contribution and do not match lexically.
    assert!(
        !has_fact(&bundle, "beta widget 0"),
        "distractors about an unmentioned entity are not surfaced: {}",
        bundle.rendered,
    );
}

#[tokio::test]
async fn without_the_high_precision_path_the_same_fact_is_missed() {
    // History mode does not fire the §4 path: the fact dense signal is a plain global ANN
    // pass. The far target ranks past the fan-out behind the near distractors, and it has
    // no lexical overlap with the query, so it does not surface — the contrast that
    // isolates the precision win above.
    let r = retriever(
        precision_store(),
        FakeEmbedder::new(&[("acme", [1.0, 0.0, 0.0, 0.0])]),
    );
    let bundle = recall(&r, "acme", TemporalMode::History, false).await;

    assert!(
        !has_fact(&bundle, "alpha widget"),
        "a plain ANN pass misses the far target: {}",
        bundle.rendered,
    );
    assert!(
        has_fact(&bundle, "beta widget 0") || has_fact(&bundle, "beta widget 1"),
        "the near distractors are what a plain pass returns: {}",
        bundle.rendered,
    );
}

#[tokio::test]
async fn an_unavailable_embedder_degrades_to_the_lexical_recall_floor() {
    // With the embedder down there is no query vector, so neither entity resolution nor
    // any dense signal runs. The fact lexical signal over the current set is the recall
    // floor: a current fact whose statement matches the query text still surfaces.
    let store = store();
    let (acme, acme_node) = entity(&store, "acme", [1.0, 0.0, 0.0, 0.0]);
    assert_fact(
        &store,
        &acme,
        acme_node,
        "acme based in berlin",
        [1.0, 0.0, 0.0, 0.0],
    );
    let r = retriever(store, FakeEmbedder::down(&[]));

    let bundle = recall(&r, "berlin", TemporalMode::Current, false).await;
    assert!(
        !bundle.explanation.embedder_available,
        "the outage is flagged"
    );
    assert!(
        has_fact(&bundle, "acme based in berlin"),
        "the lexical recall floor still surfaces the current fact: {}",
        bundle.rendered,
    );
}

#[tokio::test]
async fn sensitive_queries_compose_against_provenance_and_exclude_ungrounded_facts() {
    // acme's fact has an ABOUT window but no incoming support/provenance grounding, so it
    // is in current_support_facts but NOT in provenance_current_support_facts. A sensitive
    // query composes the seed against the provenance set, so the ungrounded fact drops out;
    // a non-sensitive query keeps it. This proves the flag routes to the provenance set.
    let r = retriever(
        precision_store(),
        FakeEmbedder::new(&[("acme", [1.0, 0.0, 0.0, 0.0])]),
    );

    let standard = recall(&r, "acme", TemporalMode::Current, false).await;
    assert!(
        has_fact(&standard, "alpha widget"),
        "a non-sensitive query keeps the ungrounded current fact"
    );

    let sensitive = recall(&r, "acme", TemporalMode::Current, true).await;
    assert!(
        !has_fact(&sensitive, "alpha widget"),
        "a sensitive query excludes the ungrounded fact (provenance set): {}",
        sensitive.rendered,
    );
}

#[tokio::test]
async fn sensitive_scopes_the_dense_fallback_to_provenance_too() {
    // The entity has no embedding, so the query resolves no root and the seed is None: the
    // dense fact ranking takes the no-seed fallback path. That path must still honor
    // `sensitive` — an ungrounded current fact is kept for a normal query but dropped for a
    // sensitive one, which reads the fallback against the provenance set. The fact's
    // statement does not match the query text, so only the dense path decides its presence.
    let store = store();
    let (acme, acme_node) = entity_unembedded(&store, "acme");
    assert_fact(
        &store,
        &acme,
        acme_node,
        "alpha widget",
        [1.0, 0.0, 0.0, 0.0],
    );
    let r = retriever(store, FakeEmbedder::new(&[("acme", [1.0, 0.0, 0.0, 0.0])]));

    let standard = recall(&r, "acme", TemporalMode::Current, false).await;
    assert!(
        has_fact(&standard, "alpha widget"),
        "the no-seed fallback surfaces the ungrounded current fact for a normal query"
    );

    let sensitive = recall(&r, "acme", TemporalMode::Current, true).await;
    assert!(
        !has_fact(&sensitive, "alpha widget"),
        "the no-seed fallback also reads against provenance for a sensitive query: {}",
        sensitive.rendered,
    );
}

#[tokio::test]
async fn the_rendered_view_is_byte_identical_across_runs() {
    let r = retriever(
        precision_store(),
        FakeEmbedder::new(&[("acme", [1.0, 0.0, 0.0, 0.0])]),
    );
    let first = recall(&r, "acme", TemporalMode::Current, false).await;
    let second = recall(&r, "acme", TemporalMode::Current, false).await;
    assert_eq!(
        first.rendered, second.rendered,
        "the high-precision path is deterministic (prefix-cache contract)"
    );
}

// --- Absolute relevance floor (P0a) ----------------------------------------------

/// A store with one entity and two current facts about it: one whose embedding matches a
/// query at `[1,0,0,0]` (cosine similarity ~1.0) and one orthogonal at `[0,1,0,0]`
/// (similarity ~0.0). Neither statement shares a token with the test queries, so the dense
/// signal alone surfaces them — letting an absolute dense floor cleanly separate the two.
fn floor_store() -> Arc<Store> {
    let store = store();
    let (acme, acme_node) = entity(&store, "acme", [1.0, 0.0, 0.0, 0.0]);
    assert_fact(&store, &acme, acme_node, "near match", [1.0, 0.0, 0.0, 0.0]);
    assert_fact(&store, &acme, acme_node, "far match", [0.0, 1.0, 0.0, 0.0]);
    store
}

/// Seed one LIVE core block (identity pre-pass material, 05 §4) under `Namespace::Global`,
/// which is in every reader's visible set — `recall_floored` mints a fresh agent principal per
/// call, so an agent-scoped block would not be visible. The block has no embedding: the pre-pass
/// includes it by identity, not by relevance, so the `min_relevance` floor never applies to it.
fn seed_core_block(store: &Store, content: &str, kind: BlockKind) -> Id {
    let id = Id::generate();
    let block = CoreBlock {
        identity: Identity {
            id,
            ingested_at: ts(T0),
            namespace: Namespace::Global,
            expired_at: None,
        },
        stats: stats(),
        content: content.to_string(),
        block_kind: kind,
        sensitivity: None,
        drift_baseline: None,
        embedding: None,
        embedder_model: None,
    };
    let audit = AuditEvent {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts(T0),
            namespace: Namespace::Global,
            expired_at: None,
        },
        kind: AuditKind::CoreEdit,
        subject_id: id,
        actor_id: Id::generate(),
        payload: serde_json::json!({"outcome": "created"}),
        signature: String::new(),
        occurred_at: ts(T0),
    };
    store
        .create_core_block(&block, &audit)
        .expect("create core block");
    id
}

/// Recall in History mode (a plain global ANN pass, no entity seeding) with a wide fan-out
/// and limit so both facts are well inside the considered pool — the only variable under
/// test is the per-query `min_relevance` floor.
async fn recall_floored(
    r: &HybridRetriever<FakeEmbedder>,
    text: &str,
    min_relevance: Option<f64>,
) -> RecallBundle {
    r.recall(RecallQuery {
        text: text.to_string(),
        principal: Principal::agent(Id::generate()),
        limit: 5,
        options: RecallOptions {
            temporal: TemporalMode::History,
            fanout: 10,
            min_relevance,
            ..RecallOptions::default()
        },
    })
    .await
    .expect("recall")
}

#[tokio::test]
async fn min_relevance_floor_drops_facts_below_the_threshold() {
    let r = retriever(
        floor_store(),
        FakeEmbedder::new(&[("acme", [1.0, 0.0, 0.0, 0.0])]),
    );

    // Floor OFF (the default 0.0): both the near and the orthogonal fact surface, exactly as
    // before P0a — the default path is unchanged.
    let open = recall_floored(&r, "acme", None).await;
    assert!(
        has_fact(&open, "near match"),
        "the near fact surfaces with no floor: {}",
        open.rendered,
    );
    assert!(
        has_fact(&open, "far match"),
        "the orthogonal fact also surfaces with no floor: {}",
        open.rendered,
    );

    // Floor at 0.5: the orthogonal fact (dense similarity ~0.0) is dropped while the near
    // fact (similarity ~1.0) is kept. This is the absolute relevance proxy at work, not the
    // relative rank — the far fact was a returned hit, just an honestly-irrelevant one.
    let floored = recall_floored(&r, "acme", Some(0.5)).await;
    assert!(
        has_fact(&floored, "near match"),
        "the near fact clears the floor: {}",
        floored.rendered,
    );
    assert!(
        !has_fact(&floored, "far match"),
        "the orthogonal fact is dropped by the 0.5 floor: {}",
        floored.rendered,
    );
}

#[tokio::test]
async fn an_active_min_relevance_floor_can_empty_an_off_topic_recall() {
    // The query vector is orthogonal to every stored fact, so all dense similarities clamp to
    // ~0.0. Under an active floor every candidate is dropped and the ranked tier is
    // legitimately empty — the honest "nothing here is relevant" answer P0a is meant to give,
    // rather than surfacing the best of a thin, irrelevant set.
    let r = retriever(
        floor_store(),
        FakeEmbedder::new(&[("offtopic", [0.0, 0.0, 1.0, 0.0])]),
    );
    let bundle = recall_floored(&r, "offtopic", Some(0.5)).await;
    assert!(
        !bundle
            .structured
            .iter()
            .any(|e| matches!(e, StructuredEntry::Fact(_))),
        "an off-topic query under an active floor returns no facts: {}",
        bundle.rendered,
    );
}

#[tokio::test]
async fn a_floor_that_empties_the_ranked_tier_still_surfaces_core_blocks() {
    // The companion to the empty-recall case above, and a refactor guard for the layering that
    // makes it safe: the floor only filters the fused ranked pool inside select(), while core /
    // identity blocks are assembled separately and prepended (retriever.rs:
    // `structured = core; structured.extend(selection.entries)`). So an off-topic query under an
    // active floor empties the RANKED tier yet must still surface every live core block. A future
    // refactor that moved the floor upstream of core assembly would silently drop identity blocks;
    // this test fails loudly instead.
    let store = floor_store();
    seed_core_block(&store, "always honor the redline", BlockKind::Redline);
    let r = retriever(
        store,
        FakeEmbedder::new(&[("offtopic", [0.0, 0.0, 1.0, 0.0])]),
    );
    let bundle = recall_floored(&r, "offtopic", Some(0.5)).await;

    // The ranked tier is empty — the orthogonal query clamps every fact to ~0.0, below the floor.
    assert!(
        !bundle
            .structured
            .iter()
            .any(|e| matches!(e, StructuredEntry::Fact(_))),
        "the off-topic floor empties the ranked tier: {}",
        bundle.rendered,
    );
    // ...but the identity prefix survives: the floor cannot reach a core block.
    assert!(
        bundle
            .structured
            .iter()
            .any(|e| matches!(e, StructuredEntry::CoreBlock(_))),
        "a core block survives a floor that empties the ranked tier: {}",
        bundle.rendered,
    );
    assert!(
        bundle
            .rendered
            .contains("kind=\"core\" block_kind=\"redline\"")
            && bundle.rendered.contains("always honor the redline"),
        "the surviving core block renders in the bundle: {}",
        bundle.rendered,
    );
}

#[tokio::test]
async fn a_per_query_min_relevance_overrides_the_config_default() {
    // A deployment configures an aggressive default floor of 0.9.
    let r = HybridRetriever::new(
        floor_store(),
        FakeEmbedder::new(&[("acme", [1.0, 0.0, 0.0, 0.0])]),
        RetrieverConfig {
            min_relevance: 0.9,
            ..RetrieverConfig::default()
        },
    );

    // No per-query override → the config default applies: the orthogonal fact (sim ~0.0) is
    // dropped, only the near fact (sim ~1.0 >= 0.9) survives.
    let default_floor = recall_floored(&r, "acme", None).await;
    assert!(
        has_fact(&default_floor, "near match"),
        "the near fact clears the configured default floor: {}",
        default_floor.rendered,
    );
    assert!(
        !has_fact(&default_floor, "far match"),
        "the configured default floor drops the orthogonal fact: {}",
        default_floor.rendered,
    );

    // A per-query Some(0.0) overrides the config default and turns the floor OFF — both facts
    // return, proving the `unwrap_or(config.min_relevance)` fallback wiring.
    let overridden = recall_floored(&r, "acme", Some(0.0)).await;
    assert!(
        has_fact(&overridden, "near match") && has_fact(&overridden, "far match"),
        "a per-query 0.0 overrides the config floor and restores the orthogonal fact: {}",
        overridden.rendered,
    );
}

#[tokio::test]
async fn recall_renders_honest_absolute_confidence_values() {
    // The headline P0a deliverable: an honest absolute confidence VALUE reaches the compact
    // surface — not just a correct drop/keep. Query [1,0,0,0] over floor_store: the near fact
    // (embedding == query vector) has cosine similarity ~1.0, the orthogonal far fact ~0.0. This
    // exercises the whole capture -> select -> entry -> render_compact pipeline on real
    // retriever output, and pins the engine's distance convention (similarity = 1 - distance).
    let r = retriever(
        floor_store(),
        FakeEmbedder::new(&[("acme", [1.0, 0.0, 0.0, 0.0])]),
    );
    let compact = recall_floored(&r, "acme", None).await.render_compact(false);
    assert!(
        compact.contains("confidence=\"1.0000\" confidence_band=\"high\""),
        "the near fact renders an exact high absolute confidence: {compact}",
    );
    assert!(
        compact.contains("confidence=\"0.0000\" confidence_band=\"low\""),
        "the orthogonal fact renders an exact low absolute confidence: {compact}",
    );
}

#[tokio::test]
async fn an_active_floor_drops_a_lexical_only_hit_through_the_retriever() {
    // With the embedder down there is no dense signal, so every hit is lexical-only (absent from
    // the dense map). Through the REAL retriever (not a hand-mutated bundle): a lexical match
    // surfaces with NO fabricated confidence when the floor is off, and is DROPPED entirely under
    // an active floor — the floor admits only dense-backed hits. Exercises both the select() drop
    // and the render omission end-to-end.
    let store = store();
    let (acme, acme_node) = entity(&store, "acme", [1.0, 0.0, 0.0, 0.0]);
    assert_fact(
        &store,
        &acme,
        acme_node,
        "acme based in berlin",
        [1.0, 0.0, 0.0, 0.0],
    );
    let r = retriever(store, FakeEmbedder::down(&[]));

    // Floor OFF: the lexical hit surfaces, and carries no confidence attribute (no dense
    // evidence — absence is the honest signal, never a fabricated value).
    let open = recall(&r, "berlin", TemporalMode::Current, false).await;
    assert!(
        has_fact(&open, "acme based in berlin"),
        "a lexical match surfaces with the embedder down: {}",
        open.rendered,
    );
    assert!(
        !open.render_compact(false).contains("confidence="),
        "a lexical-only hit gets no fabricated confidence: {}",
        open.render_compact(false),
    );

    // Floor ON (0.5): the lexical-only hit has no dense similarity, so it is dropped.
    let floored = r
        .recall(RecallQuery {
            text: "berlin".to_string(),
            principal: Principal::agent(Id::generate()),
            limit: 5,
            options: RecallOptions {
                temporal: TemporalMode::Current,
                min_relevance: Some(0.5),
                ..RecallOptions::default()
            },
        })
        .await
        .expect("recall");
    assert!(
        !has_fact(&floored, "acme based in berlin"),
        "an active floor drops a lexical-only hit: {}",
        floored.rendered,
    );
}
