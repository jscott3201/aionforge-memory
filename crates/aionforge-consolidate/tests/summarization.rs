//! End-to-end summarization tests (M2.T06b, write-and-consolidation §2): the fact-extraction
//! pass condenses a subject's facts into a conservative summary `Note` with `DERIVED_FROM`
//! lineage, non-lossily (raw facts and episodes remain) and idempotently, and the
//! detail-retention guard skips a summary that would drop too much specificity.
//!
//! Hermetic: a deterministic [`RuleExtractor`] reads the episode, a one-hot embedder keeps
//! each surface its own entity, and either the deterministic [`RuleSummarizer`] (faithful)
//! or a deliberately lossy stub drives the guard.

use std::future::Future;
use std::sync::Arc;

use aionforge_consolidate::{
    ConsolidationConfig, Consolidator, FactExtractionPass, PassConfig, RuleExtractor,
    RuleSummarizer,
};
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::{
    Embedder, SummarizationCluster, Summarizer, SummarizerIdentity, SummaryOutput,
};
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::time::Timestamp;
use aionforge_store::{BoundQuery, QueryResult, Store, StoreConfig, Value};

const DIM: usize = 16;

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
        "aionforge" => 1,
        "nyc" => 2,
        "rust" => 3,
        "helios" => 4,
        "selene" => 5,
        "bob" => 6,
        "seattle" => 7,
        "go" => 8,
        other => 9 + (other.bytes().map(usize::from).sum::<usize>() % (DIM - 9)),
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

/// A deliberately lossy summarizer: it always returns a body that names none of the
/// cluster's entities, so the detail-retention guard must skip it.
#[derive(Clone)]
struct LossySummarizer {
    identity: SummarizerIdentity,
}

impl LossySummarizer {
    fn new() -> Self {
        Self {
            identity: SummarizerIdentity {
                model_family: None,
                model_version: None,
                rule_version: "lossy-v1".to_string(),
            },
        }
    }
}

impl Summarizer for LossySummarizer {
    type Error = std::convert::Infallible;

    fn summarize(
        &self,
        _cluster: &SummarizationCluster,
    ) -> impl Future<Output = Result<Option<SummaryOutput>, Self::Error>> + Send {
        let out = Some(SummaryOutput {
            content: "a vague summary".to_string(),
            keywords: Vec::new(),
            context: None,
        });
        async move { Ok(out) }
    }

    fn identity(&self) -> &SummarizerIdentity {
        &self.identity
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

fn node_count(store: &Store, label: &str) -> u64 {
    // `label` is a trusted static node label; GQL cannot bind a label as a parameter.
    let query = format!("MATCH (n:{label}) RETURN count(n) AS n"); // gql-ident-ok
    scalar_count(store, BoundQuery::new(query))
}

fn note_lineage_edges(store: &Store) -> u64 {
    scalar_count(
        store,
        BoundQuery::new("MATCH (:Note)-[r:DERIVED_FROM]->(:Fact) RETURN count(r) AS n"),
    )
}

fn summarize_audits(store: &Store) -> u64 {
    scalar_count(
        store,
        BoundQuery::new("MATCH (a:AuditEvent) WHERE a.kind = $k RETURN count(a) AS n")
            .bind_str("k", "summarize")
            .expect("bind kind"),
    )
}

fn reset_to_raw(store: &Store) {
    let query =
        BoundQuery::new("MATCH (e:Episode) SET e.consolidation_state = $raw RETURN e.id AS id")
            .bind_str("raw", "raw")
            .expect("bind raw");
    store.execute(&query).expect("reset episodes to raw");
}

/// An episode that yields three facts about Alice (works_on, based_in, prefers) over four
/// distinct entities — enough to clear the default summarization size gates.
const ALICE_EPISODE: &str = "Alice works on Aionforge. Alice is based in NYC. Alice prefers Rust.";

#[tokio::test]
async fn summarization_writes_a_note_with_lineage_and_is_idempotent() {
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());
    insert_raw_episode(&store, ALICE_EPISODE, &namespace, 0);

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(AxisEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        PassConfig::default(),
    )));

    drain(&consolidator).await;

    // One summary note, derived from the three source facts; the raw facts and episode are
    // untouched (summarization is non-lossy).
    assert_eq!(node_count(&store, "Note"), 1, "one summary note written");
    assert_eq!(node_count(&store, "Fact"), 3, "the three raw facts remain");
    assert_eq!(node_count(&store, "Episode"), 1, "the episode is untouched");
    assert_eq!(
        note_lineage_edges(&store),
        3,
        "the note derives from each of its three source facts"
    );
    assert_eq!(
        summarize_audits(&store),
        1,
        "the written summary records one summarize audit"
    );

    // Replay: re-consolidating the same episode produces the same content-addressed note id,
    // so nothing new is written.
    reset_to_raw(&store);
    drain(&consolidator).await;
    assert_eq!(
        node_count(&store, "Note"),
        1,
        "replay writes no second note"
    );
    assert_eq!(
        note_lineage_edges(&store),
        3,
        "replay adds no duplicate lineage edge"
    );
    assert_eq!(
        node_count(&store, "Fact"),
        3,
        "replay preserves all raw facts (non-lossy)"
    );
}

#[tokio::test]
async fn a_later_episode_grows_the_cluster_into_a_new_note_keeping_the_old() {
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(AxisEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        PassConfig::default(),
    )));

    // E1: three facts about Alice -> one note over that three-fact set.
    insert_raw_episode(&store, ALICE_EPISODE, &namespace, 0);
    drain(&consolidator).await;
    assert_eq!(node_count(&store, "Note"), 1, "E1 writes the first note");
    assert_eq!(node_count(&store, "Fact"), 3);

    // E2: two more facts about Alice. The cluster now spans the committed three plus the two
    // new facts — a different source set, so a new note id; the old note is kept (non-lossy).
    insert_raw_episode(
        &store,
        "Alice uses Helios. Alice works on Selene.",
        &namespace,
        5,
    );
    drain(&consolidator).await;
    assert_eq!(
        node_count(&store, "Note"),
        2,
        "the grown cluster writes a second, distinct note; the first remains"
    );
    assert_eq!(node_count(&store, "Fact"), 5, "all five raw facts coexist");

    // Replaying every episode regenerates the same five-fact cluster id, so no third note is
    // written — cross-episode summarization is idempotent.
    reset_to_raw(&store);
    drain(&consolidator).await;
    assert_eq!(
        node_count(&store, "Note"),
        2,
        "replay adds no third note across episodes"
    );
    assert_eq!(node_count(&store, "Fact"), 5, "replay preserves every fact");
}

#[tokio::test]
async fn a_multi_subject_episode_summarizes_each_subject_separately() {
    let store = store();
    let namespace = Namespace::Agent("team".to_string());
    insert_raw_episode(
        &store,
        "Alice works on Aionforge. Alice is based in NYC. Alice prefers Rust. \
         Bob works on Aionforge. Bob is based in Seattle. Bob prefers Go.",
        &namespace,
        0,
    );

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(AxisEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        PassConfig::default(),
    )));

    drain(&consolidator).await;

    // One note per subject (Alice and Bob each clear the size gates), each deriving from its
    // own three facts — the touched-subject scoping keeps the clusters separate.
    assert_eq!(
        node_count(&store, "Note"),
        2,
        "each subject is summarized into its own note"
    );
    assert_eq!(
        note_lineage_edges(&store),
        6,
        "three lineage edges per subject note"
    );
}

#[tokio::test]
async fn the_detail_retention_guard_skips_an_over_summarized_note() {
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());
    insert_raw_episode(&store, ALICE_EPISODE, &namespace, 0);

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(AxisEmbedder::new()),
        Arc::new(LossySummarizer::new()),
        PassConfig::default(),
    )));

    drain(&consolidator).await;

    // The lossy summary names none of the cluster's entities, so the guard skips it: no note
    // is written, the raw facts remain, and the skip is audited for operator visibility.
    assert_eq!(
        node_count(&store, "Note"),
        0,
        "an over-summarized note is not written"
    );
    assert_eq!(node_count(&store, "Fact"), 3, "the raw facts are kept");
    assert_eq!(
        summarize_audits(&store),
        1,
        "the skip still records a summarize audit"
    );
}
