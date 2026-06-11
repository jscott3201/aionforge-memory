//! The M6.T01 acceptance probe (07 §3, plan M6.T01): same-family consolidation is
//! refused (or warned, per config) **at the substrate** — through the full stack of
//! `Memory::capture` (which records the writer's family in signed-or-recorded
//! provenance), background consolidation, and the `Memory::distill` /
//! `Memory::evolve_links` facade, with the guard mode coming from `MemoryConfig`,
//! not from anything the caller passes per call. M6.S2's red-team suite (and
//! M6.T05's same-family control) extends exactly this shape.
//!
//! Hermetic — fake embedder, fake summarizer/evolver with declared families.

use std::future::Future;
use std::time::Duration;

use aionforge_domain::contracts::{
    EvolvedLink, LinkEvolver, LinkEvolverIdentity, SummarizationCluster, Summarizer,
    SummarizerIdentity, SummaryOutput,
};
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::episodic::Role;
use aionforge_domain::time::Timestamp;
use aionforge_engine::{
    CaptureRequest, CaptureVerdict, ConsolidationConfig, ConsolidationGuardPolicy,
    DistillationConfig, GuardMode, LinkEvolveConfig, Memory, MemoryConfig, PassConfig,
    RuleExtractor, RuleInducer, RuleSummarizer, SummarizationConfig, WriterContext,
};
use aionforge_store::{BoundQuery, QueryResult, Value};

const CONTENT: &str = "Alice works on Aionforge. Alice is based in NYC. Alice prefers Rust.";
const SECOND: &str = "Bob works on Selene. Bob is based in Austin. Bob prefers Rust.";

fn now() -> Timestamp {
    "2026-06-10T09:00:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn later() -> Timestamp {
    "2026-06-10T10:00:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
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
struct NeverError;
impl std::fmt::Display for NeverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unreachable")
    }
}
impl std::error::Error for NeverError {}

impl aionforge_domain::contracts::Embedder for FakeEmbedder {
    type Error = NeverError;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        // One-hot on the first byte, so distinct contents land on distinct axes
        // (no spurious near-duplicate dedup) while staying deterministic.
        let out: Vec<Embedding> = inputs
            .iter()
            .map(|text| {
                let axis = usize::from(*text.as_bytes().first().unwrap_or(&0)) % 4;
                let mut v = vec![0.0f32; 4];
                v[axis] = 1.0;
                Embedding::new(v).expect("valid")
            })
            .collect();
        async move { Ok(out) }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

/// A summarizer that condenses faithfully, declaring a chosen family.
struct FakeSummarizer {
    identity: SummarizerIdentity,
}

impl FakeSummarizer {
    fn with_family(family: &str) -> Self {
        Self {
            identity: SummarizerIdentity {
                model_family: Some(family.to_string()),
                model_version: Some("1".to_string()),
                rule_version: "probe-distill-v1".to_string(),
            },
        }
    }
}

impl Summarizer for FakeSummarizer {
    type Error = NeverError;

    fn summarize(
        &self,
        cluster: &SummarizationCluster,
    ) -> impl Future<Output = Result<Option<SummaryOutput>, Self::Error>> + Send {
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

/// An evolver proposing one link to the first candidate, declaring a chosen family.
struct FakeEvolver {
    identity: LinkEvolverIdentity,
}

impl FakeEvolver {
    fn with_family(family: &str) -> Self {
        Self {
            identity: LinkEvolverIdentity {
                model_family: Some(family.to_string()),
                model_version: Some("1".to_string()),
                rule_version: "probe-evolve-v1".to_string(),
            },
        }
    }
}

impl LinkEvolver for FakeEvolver {
    type Error = NeverError;

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

fn memory_with_mode(mode: GuardMode) -> Memory<FakeEmbedder> {
    let config = MemoryConfig {
        consolidation_guard: ConsolidationGuardPolicy {
            mode,
            declared_consolidator_family: None,
        },
        ..MemoryConfig::default()
    };
    Memory::open_in_memory(FakeEmbedder::new(), &now(), config).expect("open memory")
}

/// Capture one turn through the real path, recording `family` as the writer.
async fn capture_as(memory: &Memory<FakeEmbedder>, family: &str, agent: Id) -> Namespace {
    capture_text(memory, family, agent, CONTENT).await
}

async fn capture_text(
    memory: &Memory<FakeEmbedder>,
    family: &str,
    agent: Id,
    content: &str,
) -> Namespace {
    let receipt = memory
        .capture(CaptureRequest {
            content: content.to_string(),
            role: Role::User,
            agent_id: agent,
            teams: Vec::new(),
            session_id: None,
            captured_at: now(),
            writer: WriterContext {
                model_family: family.to_string(),
                model_version: None,
                transport: None,
                request_id: None,
                trust: 0.9,
                signed: None,
            },
            trusted: false,
            namespace: None,
            supersedes: None,
        })
        .await
        .expect("capture");
    assert_eq!(receipt.verdict, CaptureVerdict::New);
    receipt.namespace.clone()
}

/// Run background consolidation until the captured episode yields current facts.
async fn consolidate(memory: &Memory<FakeEmbedder>) {
    let handle = memory.start_consolidation(
        RuleExtractor::with_default_rules(),
        RuleSummarizer::with_default_rules(),
        RuleInducer::with_default_rules(),
        ConsolidationConfig {
            tick_interval: Duration::from_millis(25),
            ..ConsolidationConfig::default()
        },
        // Cursor summarization off, so every Note in the store is a distilled one
        // and the probe's note counts measure the guard alone.
        PassConfig {
            summarization: SummarizationConfig {
                enabled: false,
                ..SummarizationConfig::default()
            },
            ..PassConfig::default()
        },
    );
    let mut derived = false;
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        if count(memory, "MATCH (f:Fact) RETURN count(f) AS n") >= 1 {
            derived = true;
            break;
        }
    }
    handle.shutdown().await;
    assert!(derived, "consolidation derived the facts");
}

fn count(memory: &Memory<FakeEmbedder>, pattern: &str) -> u64 {
    match memory
        .store()
        .execute(&BoundQuery::new(pattern))
        .expect("count")
    {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Uint(n)) => *n,
            Some(Value::Int(n)) => u64::try_from(*n).unwrap_or(0),
            _ => 0,
        },
        _ => 0,
    }
}

fn guard_payloads(memory: &Memory<FakeEmbedder>) -> Vec<String> {
    let query = BoundQuery::new("MATCH (a:AuditEvent) WHERE a.kind = $k RETURN a.payload AS p")
        .bind_str("k", "subliminal_guard_warning")
        .expect("bind kind");
    match memory.store().execute(&query).expect("payloads") {
        QueryResult::Rows(rows) => (0..rows.row_count())
            .map(|i| format!("{:?}", rows.value(i, 0)))
            .collect(),
        _ => Vec::new(),
    }
}

fn distill_config() -> DistillationConfig {
    DistillationConfig {
        enabled: true,
        ..DistillationConfig::default()
    }
}

#[tokio::test]
async fn case_a_same_family_distillation_is_refused_at_the_substrate() {
    let memory = memory_with_mode(GuardMode::Refuse);
    let ns = capture_as(&memory, "claude", Id::generate()).await;
    consolidate(&memory).await;

    let report = memory
        .distill(
            FakeSummarizer::with_family("claude-sonnet-4-6"),
            &ns,
            distill_config(),
            &later(),
        )
        .await
        .expect("distill");

    assert!(report.guard_refused >= 1, "the guard refused the cluster");
    assert_eq!(report.notes_written, 0);
    assert_eq!(
        count(&memory, "MATCH (n:Note) RETURN count(n) AS n"),
        0,
        "no distilled note reached the store"
    );
    let payloads = guard_payloads(&memory);
    assert!(
        !payloads.is_empty()
            && payloads
                .iter()
                .all(|p| p.contains("refused") && p.contains("same_family")),
        "the refusal is audited with its reason: {payloads:?}"
    );
}

#[tokio::test]
async fn case_b_warn_mode_condenses_and_audits() {
    let memory = memory_with_mode(GuardMode::Warn);
    let ns = capture_as(&memory, "claude", Id::generate()).await;
    consolidate(&memory).await;

    let report = memory
        .distill(
            FakeSummarizer::with_family("claude-sonnet-4-6"),
            &ns,
            distill_config(),
            &later(),
        )
        .await
        .expect("distill");

    assert_eq!(report.guard_refused, 0);
    assert!(report.notes_written >= 1, "warn mode writes the note");
    let payloads = guard_payloads(&memory);
    assert!(
        payloads.iter().any(|p| p.contains("warned")),
        "the finding is still audited: {payloads:?}"
    );
}

#[tokio::test]
async fn case_c_cross_family_distillation_is_untouched() {
    let memory = memory_with_mode(GuardMode::Refuse);
    let ns = capture_as(&memory, "gpt-5", Id::generate()).await;
    consolidate(&memory).await;

    let report = memory
        .distill(
            FakeSummarizer::with_family("claude-sonnet-4-6"),
            &ns,
            distill_config(),
            &later(),
        )
        .await
        .expect("distill");

    assert_eq!(report.guard_refused, 0);
    assert!(report.notes_written >= 1, "clean work is untouched");
    assert!(
        guard_payloads(&memory).is_empty(),
        "no guard row for a clean pass"
    );
}

#[tokio::test]
async fn case_e_the_two_hop_launder_is_refused_at_link_evolution() {
    // A legitimate cross-family distillation first: gpt-5 writers condensed by a
    // claude-family model. The notes now carry claude as their authoring model.
    let memory = memory_with_mode(GuardMode::Refuse);
    let agent = Id::generate();
    let ns = capture_as(&memory, "gpt-5", agent).await;
    // A second subject in the same namespace, so the evolver has a candidate pair.
    capture_text(&memory, "gpt-5", agent, SECOND).await;
    consolidate(&memory).await;
    let report = memory
        .distill(
            FakeSummarizer::with_family("claude-sonnet-4-6"),
            &ns,
            distill_config(),
            &later(),
        )
        .await
        .expect("distill");
    assert!(report.notes_written >= 2, "two clusters condensed");

    // The launder: evolve links over those notes with the SAME family that
    // distilled them. The underlying episode writers (gpt-5) differ, but the
    // note's own author is claude — the union must catch it.
    let evolve = memory
        .evolve_links(
            FakeEvolver::with_family("claude"),
            &ns,
            LinkEvolveConfig {
                enabled: true,
                ..LinkEvolveConfig::default()
            },
            &later(),
        )
        .await
        .expect("evolve");

    assert!(evolve.guard_refused >= 1, "the launder is refused");
    assert_eq!(evolve.links_created, 0);
    assert_eq!(
        count(
            &memory,
            "MATCH (:Note)-[r:RELATES_TO]->(:Note) RETURN count(r) AS n"
        ),
        0,
        "no edge was drawn"
    );
    let payloads = guard_payloads(&memory);
    assert!(
        payloads
            .iter()
            .any(|p| p.contains("link_evolve") && p.contains("same_family")),
        "audited against the link-evolve rule: {payloads:?}"
    );

    // A genuinely foreign evolver passes the same notes clean.
    let clean = memory
        .evolve_links(
            FakeEvolver::with_family("qwen-3"),
            &ns,
            LinkEvolveConfig {
                enabled: true,
                ..LinkEvolveConfig::default()
            },
            &later(),
        )
        .await
        .expect("evolve clean");
    assert_eq!(clean.guard_refused, 0, "a foreign family is not laundering");
}
