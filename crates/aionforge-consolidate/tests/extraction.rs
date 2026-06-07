//! Acceptance tests for fact extraction and entity resolution (M2.T04, write-and-
//! consolidation §2): derived facts carry provenance and source spans, many surface
//! forms resolve to one entity with audited decisions, and replaying a cursor position
//! writes nothing new (idempotent extraction).
//!
//! Hermetic: a deterministic [`RuleExtractor`] reads the episode text and a controllable
//! fake embedder makes resolution decidable — name variants that should coref or cluster
//! map to the same vector, while genuinely distinct entities map apart.

use std::future::Future;
use std::sync::Arc;

use aionforge_consolidate::{
    ConsolidationConfig, Consolidator, FactExtractionPass, PassConfig, RuleExtractor,
    RuleSummarizer,
};
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::semantic::Extraction;
use aionforge_domain::time::Timestamp;
use aionforge_store::{BoundQuery, QueryResult, Store, StoreConfig, Value};

const DIM: usize = 16;

/// A controllable embedder: it maps each surface to a one-hot vector chosen by a small
/// fixed table, so the resolution test is fully deterministic — `Bob`/`Robert` share an
/// axis (so they cluster), every other entity gets its own axis (so they stay distinct),
/// and free text spreads across the remaining axes.
#[derive(Clone)]
struct ClusterEmbedder {
    model: EmbedderModel,
}

impl ClusterEmbedder {
    fn new() -> Self {
        Self {
            model: EmbedderModel {
                family: "cluster-fake".to_string(),
                version: "1".to_string(),
                dimension: DIM as u32,
            },
        }
    }
}

fn axis(text: &str) -> usize {
    match text.trim().to_lowercase().as_str() {
        "bob" | "robert" => 0,
        "aionforge" => 2,
        "selene" => 3,
        "helios" => 4,
        "alice" | "alice smith" => 5,
        "rust" => 6,
        other => 7 + (other.bytes().map(usize::from).sum::<usize>() % (DIM - 7)),
    }
}

fn one_hot(index: usize) -> Embedding {
    let mut components = vec![0.0f32; DIM];
    components[index] = 1.0;
    Embedding::new(components).expect("non-empty finite vector")
}

impl Embedder for ClusterEmbedder {
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

/// Insert a `raw` episode. `minute` makes `ingested_at` strictly increasing so discovery
/// (and therefore cross-episode resolution) runs in a deterministic, documented order.
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
        // Guard against a stuck queue: stop if a tick made no progress at all.
        assert!(
            report.consolidated + report.retried + report.failed > 0,
            "a tick made no progress but work remains: {report:?}"
        );
    }
}

fn count(store: &Store, query: BoundQuery) -> usize {
    match store.execute(&query).expect("count query") {
        QueryResult::Rows(rows) => rows.row_count(),
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The extraction provenance of the first fact with the given predicate.
fn fact_extraction(store: &Store, predicate: &str) -> Extraction {
    let query = BoundQuery::new(
        "MATCH (f:Fact) WHERE f.predicate = $p RETURN f.extraction AS extraction LIMIT 1",
    )
    .bind_str("p", predicate)
    .expect("bind predicate");
    match store.execute(&query).expect("extraction query") {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Json(json)) => {
                serde_json::from_value(json.as_serde().clone()).expect("decode extraction")
            }
            other => panic!("expected a JSON extraction, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Every Person entity as `(canonical_name, aliases)`.
fn person_entities(store: &Store) -> Vec<(String, Vec<String>)> {
    let query = BoundQuery::new(
        "MATCH (e:Entity) WHERE e.type = $t RETURN e.canonical_name AS name, e.aliases AS aliases",
    )
    .bind_str("t", "Person")
    .expect("bind type");
    let QueryResult::Rows(rows) = store.execute(&query).expect("person query") else {
        panic!("expected rows");
    };
    let mut out = Vec::new();
    for row in 0..rows.row_count() {
        let Some(Value::String(name)) = rows.value(row, 0) else {
            continue;
        };
        let aliases = match rows.value(row, 1) {
            Some(Value::List(items)) => items
                .iter()
                .filter_map(|value| match value {
                    Value::String(alias) => Some(alias.as_str().to_string()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        out.push((name.as_str().to_string(), aliases));
    }
    out
}

fn reset_to_raw(store: &Store) {
    let query =
        BoundQuery::new("MATCH (e:Episode) SET e.consolidation_state = $raw RETURN e.id AS id")
            .bind_str("raw", "raw")
            .expect("bind raw");
    store.execute(&query).expect("reset episodes to raw");
}

#[tokio::test]
async fn extraction_resolves_surfaces_records_provenance_and_is_idempotent() {
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());

    // Episode 1 exercises intra-episode coref ("Alice" / "Alice Smith") and in-batch
    // triple dedup (the same triple twice). Episode 2 reuses "Alice" (exact-alias gate
    // across episodes). Episodes 3 and 4 exercise embedding clustering: "Robert" clusters
    // into "Bob", and the repeated triple collapses with a second supporting episode.
    insert_raw_episode(
        &store,
        "Alice works on Aionforge. Alice Smith works on Aionforge.",
        &namespace,
        0,
    );
    insert_raw_episode(&store, "Alice prefers Rust.", &namespace, 1);
    insert_raw_episode(&store, "Bob works on Helios.", &namespace, 2);
    insert_raw_episode(&store, "Robert works on Helios.", &namespace, 3);

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(ClusterEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        PassConfig::default(),
    )));

    drain(&consolidator).await;

    // 1. Derived facts carry extraction provenance and source spans (02 §6.2).
    let extraction = fact_extraction(&store, "prefers");
    assert_eq!(
        extraction.extraction_rule_version.as_deref(),
        Some("rule-v1"),
        "the fact records the extraction rule version"
    );
    assert!(
        !extraction.source_spans.is_empty(),
        "the fact carries the source spans it was drawn from"
    );

    // 2. Many surface forms resolve to one entity, with audited decisions (02 §4.3).
    let persons = person_entities(&store);
    assert_eq!(
        persons.len(),
        2,
        "Alice/Alice Smith and Bob/Robert collapse to two people: {persons:?}"
    );
    let alice = persons
        .iter()
        .find(|(name, _)| name == "Alice")
        .expect("Alice is canonical");
    assert!(
        alice.1.iter().any(|alias| alias == "Alice Smith"),
        "Alice Smith folds in as an alias: {alice:?}"
    );
    assert!(
        persons.iter().any(|(name, _)| name == "Bob"),
        "Bob is canonical (processed before Robert)"
    );
    assert!(
        !persons
            .iter()
            .any(|(name, _)| name == "Robert" || name == "Alice Smith"),
        "a folded surface is never its own entity: {persons:?}"
    );
    // One canonicalize decision is audited per distinct surface per episode: ep1 audits
    // Alice/Aionforge/Alice Smith (3), ep2 Alice/Rust (2), ep3 Bob/Helios (2), ep4
    // Robert/Helios (2) = 9. An exact count catches a regression that drops auditing for
    // whole episodes as well as one that over-audits.
    let canonicalize_audits = count(
        &store,
        BoundQuery::new("MATCH (a:AuditEvent) WHERE a.kind = $k RETURN a.id AS id")
            .bind_str("k", "canonicalize")
            .expect("bind kind"),
    );
    assert_eq!(
        canonicalize_audits, 9,
        "every resolution decision is audited, once per surface per episode"
    );

    let facts_before = count(&store, BoundQuery::new("MATCH (f:Fact) RETURN f.id AS id"));
    let entities_before = count(
        &store,
        BoundQuery::new("MATCH (e:Entity) RETURN e.id AS id"),
    );
    assert_eq!(
        facts_before, 3,
        "duplicate triples across surfaces and episodes collapse to three facts"
    );
    assert_eq!(
        entities_before, 5,
        "five distinct entities: Alice, Aionforge, Rust, Bob, Helios"
    );

    // 3. Extraction is idempotent per cursor position: replay every episode and assert
    // nothing new is written (content-derived ids plus value dedup make it a no-op).
    reset_to_raw(&store);
    drain(&consolidator).await;
    assert_eq!(
        count(&store, BoundQuery::new("MATCH (f:Fact) RETURN f.id AS id")),
        facts_before,
        "re-extraction adds no facts"
    );
    assert_eq!(
        count(
            &store,
            BoundQuery::new("MATCH (e:Entity) RETURN e.id AS id")
        ),
        entities_before,
        "re-extraction adds no entities"
    );
}
