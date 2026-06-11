//! Driver-level acceptance for the cross-family guard wiring (07 §3, M6.T01): the
//! distiller and the link-evolution pass refuse (or warn through) a same-family or
//! unverifiable item BEFORE the model call, write the `subliminal_guard_warning`
//! audit row, and leave clean cross-family work untouched. The substrate-level
//! probe through the engine facade is the M6.T01 acceptance test; these pin the
//! enforcement point itself.

use std::future::Future;
use std::sync::Arc;

use aionforge_consolidate::{
    ConsolidationConfig, Consolidator, DistillationConfig, Distiller, FactExtractionPass,
    LinkEvolveConfig, LinkEvolvePass, PassConfig, RuleExtractor, RuleSummarizer,
    SummarizationConfig,
};
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::{
    EvolvedLink, LinkEvolver, LinkEvolverIdentity, SummarizationCluster, Summarizer,
    SummarizerIdentity, SummaryOutput,
};
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Origin, Role};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::Timestamp;
use aionforge_security::GuardMode;
use aionforge_store::{
    BoundQuery, DistilledNoteWrite, MaterializedNote, QueryResult, Store, StoreConfig, Value,
};

const DIM: usize = 4;
const EPISODE: &str = "Alice works on Aionforge. Alice is based in NYC. Alice prefers Rust.";

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

fn now() -> Timestamp {
    ts("2026-06-10T12:00:00-05:00[America/Chicago]")
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

#[derive(Clone)]
struct FlatEmbedder {
    model: EmbedderModel,
}

impl FlatEmbedder {
    fn new() -> Self {
        Self {
            model: EmbedderModel {
                family: "flat-fake".to_string(),
                version: "1".to_string(),
                dimension: DIM as u32,
            },
        }
    }
}

impl aionforge_domain::contracts::Embedder for FlatEmbedder {
    type Error = std::convert::Infallible;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let out: Vec<Embedding> = inputs
            .iter()
            .map(|_| Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("valid"))
            .collect();
        async move { Ok(out) }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

/// A summarizer that always condenses faithfully, declaring a chosen family.
struct FakeSummarizer {
    identity: SummarizerIdentity,
}

impl FakeSummarizer {
    fn with_family(family: &str) -> Self {
        Self {
            identity: SummarizerIdentity {
                model_family: Some(family.to_string()),
                model_version: Some("1".to_string()),
                rule_version: "fake-distill-v1".to_string(),
            },
        }
    }
}

impl Summarizer for FakeSummarizer {
    type Error = std::convert::Infallible;

    fn summarize(
        &self,
        cluster: &SummarizationCluster,
    ) -> impl Future<Output = Result<Option<SummaryOutput>, Self::Error>> + Send {
        // A faithful echo naming every entity, so detail retention always passes
        // and the only thing deciding the outcome is the guard.
        let content = format!(
            "{}: {}",
            cluster.entity_names.join(", "),
            cluster
                .facts
                .iter()
                .map(|f| f.statement.clone())
                .collect::<Vec<_>>()
                .join(" ")
        );
        let keywords = cluster.entity_names.clone();
        async move {
            Ok(Some(SummaryOutput {
                content,
                keywords,
                context: None,
            }))
        }
    }

    fn identity(&self) -> &SummarizerIdentity {
        &self.identity
    }
}

/// An evolver that always proposes a link to the first candidate, declaring a
/// chosen family.
struct FakeEvolver {
    identity: LinkEvolverIdentity,
}

impl FakeEvolver {
    fn with_family(family: &str) -> Self {
        Self {
            identity: LinkEvolverIdentity {
                model_family: Some(family.to_string()),
                model_version: Some("1".to_string()),
                rule_version: "fake-evolve-v1".to_string(),
            },
        }
    }
}

impl LinkEvolver for FakeEvolver {
    type Error = std::convert::Infallible;

    fn evolve(
        &self,
        _source: &Note,
        candidates: &[Note],
    ) -> impl Future<Output = Result<Option<Vec<EvolvedLink>>, Self::Error>> + Send {
        let links = candidates.first().map(|c| {
            vec![EvolvedLink {
                target_id: c.identity.id,
                relationship_label: "related_to".to_string(),
                confidence: 0.9,
            }]
        });
        async move { Ok(links) }
    }

    fn identity(&self) -> &LinkEvolverIdentity {
        &self.identity
    }
}

/// Insert a raw episode whose origin declares `family` as the writer (or nothing).
fn insert_episode(store: &Store, ns: &Namespace, family: Option<&str>) {
    let at = ts("2026-06-10T09:00:00-05:00[America/Chicago]");
    let episode = Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: at.clone(),
            namespace: ns.clone(),
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
        content: EPISODE.to_string(),
        role: Role::User,
        captured_at: at,
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(EPISODE.as_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: family.map(|f| Origin {
            model_family: Some(f.to_string()),
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

/// Consolidate the seeded episode into current facts (cursor summarization off).
async fn populate_facts(store: &Arc<Store>, ns: &Namespace, family: Option<&str>) {
    insert_episode(store, ns, family);
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
        Arc::new(FlatEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        pass_config,
    )));
    loop {
        let report = consolidator.tick_once().await.expect("tick");
        if report.pending_after == 0 {
            break;
        }
    }
}

fn distill_config() -> DistillationConfig {
    DistillationConfig {
        enabled: true,
        ..DistillationConfig::default()
    }
}

fn count(store: &Store, pattern: &str) -> u64 {
    match store.execute(&BoundQuery::new(pattern)).expect("count") {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Uint(n)) => *n,
            Some(Value::Int(n)) => u64::try_from(*n).unwrap_or(0),
            _ => 0,
        },
        _ => 0,
    }
}

fn note_count(store: &Store) -> u64 {
    count(store, "MATCH (n:Note) RETURN count(n) AS n")
}

fn guard_rows(store: &Store) -> u64 {
    let query = BoundQuery::new("MATCH (a:AuditEvent) WHERE a.kind = $k RETURN count(a) AS n")
        .bind_str("k", "subliminal_guard_warning")
        .expect("bind kind");
    match store.execute(&query).expect("count") {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Uint(n)) => *n,
            Some(Value::Int(n)) => u64::try_from(*n).unwrap_or(0),
            _ => 0,
        },
        _ => 0,
    }
}

fn guard_payloads(store: &Store) -> Vec<String> {
    let query = BoundQuery::new("MATCH (a:AuditEvent) WHERE a.kind = $k RETURN a.payload AS p")
        .bind_str("k", "subliminal_guard_warning")
        .expect("bind kind");
    match store.execute(&query).expect("payloads") {
        QueryResult::Rows(rows) => (0..rows.row_count())
            .map(|i| format!("{:?}", rows.value(i, 0)))
            .collect(),
        _ => Vec::new(),
    }
}

/// Seed a live, embedded note whose `Distill` audit declares `distilled_by` as the
/// authoring model (the two-hop launder surface).
fn seed_note(store: &Store, seed: &[u8], ns: &Namespace, distilled_by: &str) -> Id {
    let id = Id::from_content_hash(seed);
    let note = Note {
        identity: Identity {
            id,
            ingested_at: now(),
            namespace: ns.clone(),
            expired_at: None,
        },
        stats: Stats {
            importance: 0.5,
            trust: 0.8,
            last_access: now(),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.0,
            is_pinned: false,
        },
        content: format!("note {}", String::from_utf8_lossy(seed)),
        context: None,
        keywords: Vec::new(),
        embedding: Some(Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("valid")),
        embedder_model: None,
        derived_from_episode: None,
    };
    let audit = AuditEvent {
        identity: Identity {
            id: Id::from_content_hash(&[seed, b"-seed-audit"].concat()),
            ingested_at: now(),
            namespace: ns.clone(),
            expired_at: None,
        },
        kind: AuditKind::Distill,
        subject_id: id,
        actor_id: Id::from_content_hash(b"seed"),
        payload: serde_json::json!({"outcome": "written", "model_family": distilled_by}),
        signature: String::new(),
        occurred_at: now(),
    };
    store
        .materialize_distilled_notes(
            &[DistilledNoteWrite {
                note: MaterializedNote {
                    note,
                    source_facts: Vec::new(),
                },
                audit,
            }],
            &[],
            &now(),
        )
        .expect("seed note");
    id
}

#[tokio::test]
async fn a_same_family_cluster_is_refused_before_the_model_and_audited() {
    let store = store();
    let ns = Namespace::Agent("alice".to_string());
    // The writers declare the bare family; the distiller declares the full id —
    // the boundary-prefix rule must still catch it.
    populate_facts(&store, &ns, Some("claude")).await;

    let distiller = Distiller::new(
        FakeSummarizer::with_family("claude-sonnet-4-6"),
        Arc::new(FlatEmbedder::new()),
        distill_config(),
        GuardMode::Refuse,
    );
    let report = distiller.distill(&store, &ns, &now()).await.expect("run");

    assert!(report.guard_refused >= 1, "the cluster is refused");
    assert_eq!(report.notes_written, 0, "nothing was condensed");
    assert_eq!(note_count(&store), 0, "no distilled note exists");
    assert_eq!(report.declined, 0, "the model was never consulted");
    let payloads = guard_payloads(&store);
    assert!(!payloads.is_empty(), "the refusal is audited");
    assert!(
        payloads.iter().all(|p| p.contains("refused")
            && p.contains("same_family")
            && p.contains("distill")
            && p.contains("claude")),
        "the row names the action, reason, rule, and family: {payloads:?}"
    );
}

#[tokio::test]
async fn warn_mode_condenses_anyway_but_audits_the_finding() {
    let store = store();
    let ns = Namespace::Agent("alice".to_string());
    populate_facts(&store, &ns, Some("claude")).await;

    let distiller = Distiller::new(
        FakeSummarizer::with_family("claude-sonnet-4-6"),
        Arc::new(FlatEmbedder::new()),
        distill_config(),
        GuardMode::Warn,
    );
    let report = distiller.distill(&store, &ns, &now()).await.expect("run");

    assert_eq!(report.guard_refused, 0, "warn mode refuses nothing");
    assert!(report.notes_written >= 1, "the note is still written");
    assert!(note_count(&store) >= 1);
    let payloads = guard_payloads(&store);
    assert!(
        payloads.iter().any(|p| p.contains("warned")),
        "the finding is audited as a warning: {payloads:?}"
    );
}

#[tokio::test]
async fn an_unverifiable_writer_is_refused_fail_closed() {
    let store = store();
    let ns = Namespace::Agent("alice".to_string());
    // No origin, no provenance record, no enrolled agent: nobody can vouch.
    populate_facts(&store, &ns, None).await;

    let distiller = Distiller::new(
        FakeSummarizer::with_family("claude-sonnet-4-6"),
        Arc::new(FlatEmbedder::new()),
        distill_config(),
        GuardMode::Refuse,
    );
    let report = distiller.distill(&store, &ns, &now()).await.expect("run");

    assert!(report.guard_refused >= 1);
    assert_eq!(note_count(&store), 0);
    let payloads = guard_payloads(&store);
    assert!(
        payloads.iter().all(|p| p.contains("unverifiable_writer")),
        "unverifiable, never 'differs': {payloads:?}"
    );
}

#[tokio::test]
async fn cross_family_distillation_passes_with_no_guard_rows() {
    let store = store();
    let ns = Namespace::Agent("alice".to_string());
    populate_facts(&store, &ns, Some("gpt-5")).await;

    let distiller = Distiller::new(
        FakeSummarizer::with_family("claude-sonnet-4-6"),
        Arc::new(FlatEmbedder::new()),
        distill_config(),
        GuardMode::Refuse,
    );
    let report = distiller.distill(&store, &ns, &now()).await.expect("run");

    assert_eq!(report.guard_refused, 0);
    assert!(report.notes_written >= 1, "clean work is untouched");
    assert_eq!(guard_rows(&store), 0, "no guard row for a clean pass");
}

#[tokio::test]
async fn the_two_hop_launder_is_refused_at_link_evolution() {
    let store = store();
    let ns = Namespace::Agent("alice".to_string());
    // Both notes were distilled by "mock-large"; the evolver declares "mock" —
    // same family through the note's own authoring model, whatever the (absent)
    // underlying episode writers say.
    seed_note(&store, b"laundered-a", &ns, "mock-large");
    seed_note(&store, b"laundered-b", &ns, "mock-large");

    let pass = LinkEvolvePass::new(
        FakeEvolver::with_family("mock"),
        LinkEvolveConfig {
            enabled: true,
            ..LinkEvolveConfig::default()
        },
        GuardMode::Refuse,
    );
    let report = pass.evolve_links(&store, &ns, &now()).await.expect("run");

    assert!(report.guard_refused >= 1, "the launder is refused");
    assert_eq!(report.links_created, 0);
    assert_eq!(
        count(
            &store,
            "MATCH (:Note)-[r:RELATES_TO]->(:Note) RETURN count(r) AS n"
        ),
        0,
        "no edge was drawn"
    );
    let payloads = guard_payloads(&store);
    assert!(
        payloads
            .iter()
            .all(|p| p.contains("link_evolve") && p.contains("same_family")),
        "audited against the link-evolve rule: {payloads:?}"
    );
}

#[tokio::test]
async fn cross_family_link_evolution_passes() {
    let store = store();
    let ns = Namespace::Agent("alice".to_string());
    seed_note(&store, b"clean-a", &ns, "writer-fake");
    seed_note(&store, b"clean-b", &ns, "writer-fake");

    let pass = LinkEvolvePass::new(
        FakeEvolver::with_family("mock"),
        LinkEvolveConfig {
            enabled: true,
            ..LinkEvolveConfig::default()
        },
        GuardMode::Refuse,
    );
    let report = pass.evolve_links(&store, &ns, &now()).await.expect("run");

    assert_eq!(report.guard_refused, 0);
    assert!(report.links_created >= 1, "clean links are drawn");
    assert_eq!(guard_rows(&store), 0);
}

#[tokio::test]
async fn a_refused_cluster_audits_idempotently_across_reruns() {
    let store = store();
    let ns = Namespace::Agent("alice".to_string());
    populate_facts(&store, &ns, Some("claude")).await;

    let distiller = Distiller::new(
        FakeSummarizer::with_family("claude-sonnet-4-6"),
        Arc::new(FlatEmbedder::new()),
        distill_config(),
        GuardMode::Refuse,
    );
    distiller.distill(&store, &ns, &now()).await.expect("run");
    let after_first = guard_rows(&store);
    distiller
        .distill(
            &store,
            &ns,
            &ts("2026-06-10T13:00:00-05:00[America/Chicago]"),
        )
        .await
        .expect("re-run");
    assert_eq!(
        guard_rows(&store),
        after_first,
        "a re-run over unchanged ground dedups to the same content-addressed rows"
    );
}
