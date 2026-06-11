//! End-to-end tests for the off-cursor LLM distiller (M3.T08): the [`Distiller`] reads a
//! namespace's current support facts, condenses each subject's cluster with an
//! [`LLMSummarizer`] over a mock chat completer, and writes non-canonical distilled `Note`s with
//! `DERIVED_FROM` lineage and `distill` provenance audits — entirely off the consolidation cursor.
//!
//! Hermetic: a deterministic [`RuleExtractor`] populates the facts (with cursor summarization
//! disabled, so the only notes are the distilled ones), a one-hot embedder embeds the bodies, and
//! a [`MockCompleter`] stands in for the model with selectable behavior (faithful echo, lossy,
//! truncated, unavailable).

use std::future::Future;
use std::sync::Arc;

use aionforge_consolidate::{
    ConsolidationConfig, Consolidator, DistillationConfig, Distiller, FactExtractionPass,
    LLMSummarizer, PassConfig, RuleExtractor, RuleSummarizer, SummarizationConfig,
};
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::{Completer, Embedder};
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Origin, Role};
use aionforge_domain::time::Timestamp;
use aionforge_domain::{ChatRole, CompleterModel, Completion, CompletionRequest};
use aionforge_security::GuardMode;
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

/// A mock chat completer with selectable behavior, standing in for the model.
#[derive(Clone)]
struct MockCompleter {
    model: CompleterModel,
    behavior: Behavior,
}

#[derive(Clone, Copy)]
enum Behavior {
    /// A faithful summary naming every entity the prompt listed (clears the guard).
    Echo,
    /// A vague summary naming no entity (the guard rejects it).
    Lossy,
    /// A reply truncated at the token cap (`finish_reason == "length"`).
    Truncated,
    /// The endpoint is unavailable.
    Unavailable,
}

#[derive(Debug, thiserror::Error)]
#[error("mock completer unavailable")]
struct MockError;

/// Pull the `<entities>…</entities>` list out of the rendered prompt and un-escape it, so the
/// echo reply names exactly the entities the distiller asked about.
fn entities_in(request: &CompletionRequest) -> Vec<String> {
    let user = request
        .messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, ChatRole::User))
        .map(|m| m.content.as_str())
        .unwrap_or("");
    let Some(start) = user.find("<entities>") else {
        return Vec::new();
    };
    let rest = &user[start + "<entities>".len()..];
    let Some(end) = rest.find("</entities>") else {
        return Vec::new();
    };
    rest[..end]
        .split("; ")
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&")
        })
        .collect()
}

impl Completer for MockCompleter {
    type Error = MockError;

    fn complete(
        &self,
        request: &CompletionRequest,
    ) -> impl Future<Output = Result<Completion, Self::Error>> + Send {
        let behavior = self.behavior;
        let model = self.model.version.clone();
        let entities = entities_in(request);
        async move {
            match behavior {
                Behavior::Echo => Ok(Completion {
                    content: format!("Distilled summary covering {}.", entities.join(", ")),
                    responding_model: model,
                    finish_reason: Some("stop".to_string()),
                }),
                Behavior::Lossy => Ok(Completion {
                    content: "a vague summary of some things".to_string(),
                    responding_model: model,
                    finish_reason: Some("stop".to_string()),
                }),
                Behavior::Truncated => Ok(Completion {
                    content: format!("Distilled summary covering {}", entities.join(", ")),
                    responding_model: model,
                    finish_reason: Some("length".to_string()),
                }),
                Behavior::Unavailable => Err(MockError),
            }
        }
    }

    fn model(&self) -> &CompleterModel {
        &self.model
    }
}

fn mock(behavior: Behavior) -> LLMSummarizer<MockCompleter> {
    LLMSummarizer::new(MockCompleter {
        model: CompleterModel {
            family: "claude".to_string(),
            version: "opus-4-8".to_string(),
        },
        behavior,
    })
}

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

fn now() -> Timestamp {
    ts("2026-06-06T12:00:00-05:00[America/Chicago]")
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

fn insert_raw_episode(store: &Store, content: &str, namespace: &Namespace) {
    let at = ts("2026-06-06T09:00:00-05:00[America/Chicago]");
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
        // A verifiable writer family, distinct from the mock completer's "claude",
        // so the cross-family guard passes these fixtures (07 §3, M6.T01).
        origin: Some(Origin {
            model_family: Some("writer-fake".to_string()),
            model_version: None,
            transport: None,
            request_id: None,
            redactions: Vec::new(),
            injection_flags: Vec::new(),
            capture_latency_ms: None,
            supersedes: None,
        }),
    };
    store.insert_episode(&episode).expect("insert episode");
}

/// Consolidate the episode into facts with cursor summarization OFF, so the only `Note`s that can
/// appear afterward are the distilled ones — and the facts land in `current_support_facts`, which
/// the distiller reads.
async fn populate_facts(store: &Arc<Store>, content: &str, namespace: &Namespace) {
    insert_raw_episode(store, content, namespace);
    let pass_config = PassConfig {
        summarization: SummarizationConfig {
            enabled: false,
            ..SummarizationConfig::default()
        },
        ..PassConfig::default()
    };
    let mut consolidator = Consolidator::new(Arc::clone(store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(AxisEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        pass_config,
    )));
    loop {
        let report = consolidator.tick_once().await.expect("tick");
        if report.pending_after == 0 {
            break;
        }
        assert!(
            report.consolidated + report.retried + report.failed > 0,
            "no progress"
        );
    }
}

fn enabled_config() -> DistillationConfig {
    DistillationConfig {
        enabled: true,
        endpoint: Some("https://api.test.example/v1/messages".to_string()),
        seed: Some(42),
        ..DistillationConfig::default()
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

fn note_count(store: &Store) -> u64 {
    scalar_count(
        store,
        BoundQuery::new("MATCH (n:Note) RETURN count(n) AS n"),
    )
}

fn fact_count(store: &Store) -> u64 {
    scalar_count(
        store,
        BoundQuery::new("MATCH (f:Fact) RETURN count(f) AS n"),
    )
}

fn distill_audit_count(store: &Store) -> u64 {
    scalar_count(
        store,
        BoundQuery::new("MATCH (a:AuditEvent) WHERE a.kind = $k RETURN count(a) AS n")
            .bind_str("k", "distill")
            .expect("bind kind"),
    )
}

fn audit_to_note_edges(store: &Store) -> u64 {
    scalar_count(
        store,
        BoundQuery::new("MATCH (:AuditEvent)-[r:AUDIT]->(:Note) RETURN count(r) AS n"),
    )
}

fn lineage_edges(store: &Store) -> u64 {
    scalar_count(
        store,
        BoundQuery::new("MATCH (:Note)-[r:DERIVED_FROM]->(:Fact) RETURN count(r) AS n"),
    )
}

/// The `Debug` rendering of the first distill-audit payload, for substring assertions on the
/// recorded provenance (the exact `Value` shape of a stored JSON column is an internal detail).
fn first_distill_payload(store: &Store) -> String {
    let query = BoundQuery::new("MATCH (a:AuditEvent) WHERE a.kind = $k RETURN a.payload AS p")
        .bind_str("k", "distill")
        .expect("bind kind");
    match store.execute(&query).expect("payload query") {
        QueryResult::Rows(rows) => format!("{:?}", rows.value(0, 0)),
        other => panic!("expected rows, got {other:?}"),
    }
}

const ALICE_EPISODE: &str = "Alice works on Aionforge. Alice is based in NYC. Alice prefers Rust.";

#[tokio::test]
async fn distillation_writes_a_note_with_lineage_and_audit_to_note_provenance() {
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());
    populate_facts(&store, ALICE_EPISODE, &namespace).await;
    assert!(
        fact_count(&store) >= 3,
        "facts are consolidated and current"
    );
    assert_eq!(
        note_count(&store),
        0,
        "cursor summarization is off — no notes yet"
    );

    let distiller = Distiller::new(
        mock(Behavior::Echo),
        Arc::new(AxisEmbedder::new()),
        enabled_config(),
        GuardMode::default(),
    );
    let report = distiller
        .distill(&store, &namespace, &now())
        .await
        .expect("distill");

    assert_eq!(
        report.notes_written, 1,
        "one distilled note for the Alice cluster"
    );
    assert_eq!(note_count(&store), 1, "the distilled note is written");
    assert!(
        lineage_edges(&store) >= 3,
        "the note derives from its source facts"
    );
    assert_eq!(distill_audit_count(&store), 1, "the call is audited");
    assert!(
        first_distill_payload(&store).contains("written"),
        "audited as written"
    );
    assert_eq!(
        audit_to_note_edges(&store),
        1,
        "provenance is wired audit -> the note it produced"
    );
}

#[tokio::test]
async fn an_unavailable_model_degrades_to_the_canonical_tier() {
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());
    populate_facts(&store, ALICE_EPISODE, &namespace).await;

    let distiller = Distiller::new(
        mock(Behavior::Unavailable),
        Arc::new(AxisEmbedder::new()),
        enabled_config(),
        GuardMode::default(),
    );
    let report = distiller
        .distill(&store, &namespace, &now())
        .await
        .expect("distill");

    assert_eq!(
        report.notes_written, 0,
        "no note when the model is unavailable"
    );
    assert_eq!(
        report.declined, 1,
        "the unavailable call is recorded as declined"
    );
    assert_eq!(
        note_count(&store),
        0,
        "the canonical tier (facts) stands alone"
    );
    assert_eq!(
        distill_audit_count(&store),
        1,
        "the declined call is audited"
    );
    assert!(
        first_distill_payload(&store).contains("declined"),
        "audited as declined"
    );
}

#[tokio::test]
async fn a_lossy_summary_is_rejected_by_the_detail_retention_guard() {
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());
    populate_facts(&store, ALICE_EPISODE, &namespace).await;

    let distiller = Distiller::new(
        mock(Behavior::Lossy),
        Arc::new(AxisEmbedder::new()),
        enabled_config(),
        GuardMode::default(),
    );
    let report = distiller
        .distill(&store, &namespace, &now())
        .await
        .expect("distill");

    assert_eq!(report.notes_written, 0, "a lossy summary is not written");
    assert_eq!(report.rejected_lossy, 1, "the guard rejected it");
    assert_eq!(note_count(&store), 0);
    assert_eq!(distill_audit_count(&store), 1, "the rejection is audited");
    assert!(
        first_distill_payload(&store).contains("rejected_lossy"),
        "audited as rejected_lossy"
    );
}

#[tokio::test]
async fn a_truncated_completion_is_declined() {
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());
    populate_facts(&store, ALICE_EPISODE, &namespace).await;

    let distiller = Distiller::new(
        mock(Behavior::Truncated),
        Arc::new(AxisEmbedder::new()),
        enabled_config(),
        GuardMode::default(),
    );
    let report = distiller
        .distill(&store, &namespace, &now())
        .await
        .expect("distill");

    assert_eq!(
        report.notes_written, 0,
        "a truncated (lossy) completion writes no note"
    );
    assert_eq!(report.declined, 1);
    assert_eq!(note_count(&store), 0);
}

#[tokio::test]
async fn a_disabled_distiller_is_a_no_op() {
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());
    populate_facts(&store, ALICE_EPISODE, &namespace).await;

    let config = DistillationConfig {
        enabled: false,
        ..enabled_config()
    };
    let distiller = Distiller::new(
        mock(Behavior::Echo),
        Arc::new(AxisEmbedder::new()),
        config,
        GuardMode::default(),
    );
    let report = distiller
        .distill(&store, &namespace, &now())
        .await
        .expect("distill");

    assert_eq!(
        report,
        aionforge_consolidate::DistillationReport::default(),
        "off by default"
    );
    assert_eq!(note_count(&store), 0, "a disabled distiller writes nothing");
}

#[tokio::test]
async fn distillation_is_idempotent_on_replay() {
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());
    populate_facts(&store, ALICE_EPISODE, &namespace).await;

    let distiller = Distiller::new(
        mock(Behavior::Echo),
        Arc::new(AxisEmbedder::new()),
        enabled_config(),
        GuardMode::default(),
    );
    distiller
        .distill(&store, &namespace, &now())
        .await
        .expect("first distill");
    let first = note_count(&store);
    distiller
        .distill(&store, &namespace, &now())
        .await
        .expect("second distill");

    assert_eq!(note_count(&store), first, "replay writes no second note");
    assert_eq!(
        audit_to_note_edges(&store),
        1,
        "replay adds no second provenance edge"
    );
}

#[tokio::test]
async fn the_provenance_audit_records_model_identity_endpoint_and_seed() {
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());
    populate_facts(&store, ALICE_EPISODE, &namespace).await;

    let distiller = Distiller::new(
        mock(Behavior::Echo),
        Arc::new(AxisEmbedder::new()),
        enabled_config(),
        GuardMode::default(),
    );
    distiller
        .distill(&store, &namespace, &now())
        .await
        .expect("distill");

    let payload = first_distill_payload(&store);
    assert!(
        payload.contains("claude"),
        "records the model family: {payload}"
    );
    assert!(payload.contains("opus-4-8"), "records the model version");
    assert!(payload.contains("api.test.example"), "records the endpoint");
    assert!(payload.contains("42"), "records the seed");
    assert!(
        payload.contains("llm-distill-v1"),
        "records the distiller rule version"
    );
}

#[tokio::test]
async fn distilled_notes_coexist_with_cursor_rule_summaries_in_a_disjoint_id_space() {
    // With cursor summarization ON, the rule summary note and the distilled note coexist —
    // different rule versions put them in disjoint id-spaces, so both land.
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());

    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        Arc::new(AxisEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        PassConfig::default(),
    )));
    insert_raw_episode(&store, ALICE_EPISODE, &namespace);
    loop {
        let report = consolidator.tick_once().await.expect("tick");
        if report.pending_after == 0 {
            break;
        }
    }
    assert_eq!(
        note_count(&store),
        1,
        "the cursor wrote one canonical rule summary"
    );

    let distiller = Distiller::new(
        mock(Behavior::Echo),
        Arc::new(AxisEmbedder::new()),
        enabled_config(),
        GuardMode::default(),
    );
    distiller
        .distill(&store, &namespace, &now())
        .await
        .expect("distill");

    assert_eq!(
        note_count(&store),
        2,
        "the distilled note coexists with the rule summary (disjoint id-spaces)"
    );
}

const TEAM_EPISODE: &str = "Alice works on Aionforge. Alice is based in NYC. Alice prefers Rust. \
                            Bob works on Aionforge. Bob is based in Seattle. Bob prefers Go.";

#[tokio::test]
async fn a_multi_subject_batch_distills_each_subject_into_its_own_note() {
    let store = store();
    let namespace = Namespace::Agent("team".to_string());
    populate_facts(&store, TEAM_EPISODE, &namespace).await;

    let distiller = Distiller::new(
        mock(Behavior::Echo),
        Arc::new(AxisEmbedder::new()),
        enabled_config(),
        GuardMode::default(),
    );
    let report = distiller
        .distill(&store, &namespace, &now())
        .await
        .expect("distill");

    assert_eq!(report.clusters_seen, 2, "Alice and Bob each form a cluster");
    assert_eq!(report.notes_written, 2, "one distilled note per subject");
    assert_eq!(note_count(&store), 2);
    assert_eq!(
        audit_to_note_edges(&store),
        2,
        "each note has its own provenance edge (the batch embeds and wires all of them)"
    );
}

#[tokio::test]
async fn distillation_stays_within_its_namespace() {
    // Alice's facts live in namespace A, Bob's in namespace B. Distilling A must touch only A.
    let store = store();
    let ns_a = Namespace::Agent("alice".to_string());
    let ns_b = Namespace::Agent("bob".to_string());
    populate_facts(&store, ALICE_EPISODE, &ns_a).await;
    populate_facts(
        &store,
        "Bob works on Aionforge. Bob is based in Seattle. Bob prefers Go.",
        &ns_b,
    )
    .await;

    let distiller = Distiller::new(
        mock(Behavior::Echo),
        Arc::new(AxisEmbedder::new()),
        enabled_config(),
        GuardMode::default(),
    );
    let report = distiller
        .distill(&store, &ns_a, &now())
        .await
        .expect("distill");

    assert_eq!(
        report.clusters_seen, 1,
        "only the queried namespace's subject is seen"
    );
    assert_eq!(report.notes_written, 1, "one note, for Alice");
    assert_eq!(note_count(&store), 1, "Bob's namespace is untouched");
}

#[tokio::test]
async fn a_flapping_outcome_records_a_fresh_call_and_then_writes_the_note() {
    // The model is unavailable on the first run (declined, no note) and recovers on the second
    // (written). The audit id includes the outcome, so the two calls are both recorded; the second
    // run writes the note for the same fact set and wires exactly one provenance edge.
    let store = store();
    let namespace = Namespace::Agent("alice".to_string());
    populate_facts(&store, ALICE_EPISODE, &namespace).await;

    let down = Distiller::new(
        mock(Behavior::Unavailable),
        Arc::new(AxisEmbedder::new()),
        enabled_config(),
        GuardMode::default(),
    );
    let first = down
        .distill(&store, &namespace, &now())
        .await
        .expect("first run");
    assert_eq!(first.declined, 1, "the unavailable call is recorded");
    assert_eq!(note_count(&store), 0, "no note while the model is down");

    let up = Distiller::new(
        mock(Behavior::Echo),
        Arc::new(AxisEmbedder::new()),
        enabled_config(),
        GuardMode::default(),
    );
    let second = up
        .distill(&store, &namespace, &now())
        .await
        .expect("second run");
    assert_eq!(
        second.notes_written, 1,
        "the recovered model writes the note"
    );
    assert_eq!(note_count(&store), 1);
    assert_eq!(
        distill_audit_count(&store),
        2,
        "both calls are audited (declined, then written) — the audit id includes the outcome"
    );
    assert_eq!(
        audit_to_note_edges(&store),
        1,
        "only the written call wires a provenance edge to the note"
    );
}
