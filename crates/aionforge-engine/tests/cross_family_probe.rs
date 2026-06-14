//! The M6.T01 acceptance probe (07 §3, plan M6.T01): same-family inference-backed
//! consolidation is refused (or warned, per config) **at the substrate** — through the
//! `Memory::evolve_links` facade, with the guard mode coming from `MemoryConfig`, not from
//! anything the caller passes per call. M6.S2's red-team suite (and M6.T05's same-family
//! control) extends exactly this shape.
//!
//! The shipped consolidation path is deterministic; the off-cursor link evolver is the one
//! inference-backed seam, so the guard is probed through it with an in-test mock evolver that
//! declares a model family. (The optional LLM note-distiller was removed; its substrate-level
//! cases retired with it.) Hermetic — fake embedder, fake evolver with declared families.

use std::future::Future;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::{EvolvedLink, LinkEvolver, LinkEvolverIdentity};
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::Timestamp;
use aionforge_engine::{
    ConsolidationGuardPolicy, GuardMode, LinkEvolveConfig, Memory, MemoryConfig,
};
use aionforge_store::{BoundQuery, MaterializedNote, QueryResult, Value};

const DIM: usize = 4;

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
                dimension: DIM as u32,
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

/// Seed a live, embedded note whose `Distill` audit declares `authored_by` as its authoring
/// model — the surface the cross-family guard reads as a writer family. The note is written
/// through the surviving note materializer; `AuditKind::Distill` is no longer emitted by any
/// path, so the hand-seeded row exercises the retained by-subject lineage decode the guard reads.
fn seed_note(memory: &Memory<FakeEmbedder>, seed: &[u8], ns: &Namespace, authored_by: &str) -> Id {
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
    memory
        .store()
        .seed_notes_for_test(
            &[MaterializedNote {
                note,
                source_facts: Vec::new(),
            }],
            &now(),
        )
        .expect("seed note");

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
        payload: serde_json::json!({"outcome": "written", "model_family": authored_by}),
        signature: String::new(),
        occurred_at: now(),
    };
    memory
        .store()
        .commit_audit(&audit)
        .expect("seed distill tombstone");
    id
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

#[tokio::test]
async fn same_family_link_evolution_is_refused_at_the_substrate() {
    let memory = memory_with_mode(GuardMode::Refuse);
    let ns = Namespace::Agent("alice".to_string());
    // Both notes authored by a claude-family model; the evolver declares claude — the guard
    // catches the family match through the note's own authoring model.
    seed_note(&memory, b"probe-a", &ns, "claude");
    seed_note(&memory, b"probe-b", &ns, "claude");

    let report = memory
        .evolve_links(
            FakeEvolver::with_family("claude-sonnet-4-6"),
            &ns,
            LinkEvolveConfig {
                enabled: true,
                ..LinkEvolveConfig::default()
            },
            &later(),
        )
        .await
        .expect("evolve");

    assert!(report.guard_refused >= 1, "the guard refused the source");
    assert_eq!(report.links_created, 0);
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
        !payloads.is_empty()
            && payloads
                .iter()
                .all(|p| p.contains("refused") && p.contains("same_family")),
        "the refusal is audited with its reason: {payloads:?}"
    );
}

#[tokio::test]
async fn warn_mode_proceeds_and_audits() {
    let memory = memory_with_mode(GuardMode::Warn);
    let ns = Namespace::Agent("alice".to_string());
    seed_note(&memory, b"probe-a", &ns, "claude");
    seed_note(&memory, b"probe-b", &ns, "claude");

    let report = memory
        .evolve_links(
            FakeEvolver::with_family("claude-sonnet-4-6"),
            &ns,
            LinkEvolveConfig {
                enabled: true,
                ..LinkEvolveConfig::default()
            },
            &later(),
        )
        .await
        .expect("evolve");

    assert_eq!(report.guard_refused, 0);
    assert!(report.links_created >= 1, "warn mode draws the link");
    let payloads = guard_payloads(&memory);
    assert!(
        payloads.iter().any(|p| p.contains("warned")),
        "the finding is still audited: {payloads:?}"
    );
}

#[tokio::test]
async fn cross_family_link_evolution_is_untouched() {
    let memory = memory_with_mode(GuardMode::Refuse);
    let ns = Namespace::Agent("alice".to_string());
    seed_note(&memory, b"probe-a", &ns, "gpt-5");
    seed_note(&memory, b"probe-b", &ns, "gpt-5");

    let report = memory
        .evolve_links(
            FakeEvolver::with_family("claude-sonnet-4-6"),
            &ns,
            LinkEvolveConfig {
                enabled: true,
                ..LinkEvolveConfig::default()
            },
            &later(),
        )
        .await
        .expect("evolve");

    assert_eq!(report.guard_refused, 0);
    assert!(report.links_created >= 1, "clean work is untouched");
    assert!(
        guard_payloads(&memory).is_empty(),
        "no guard row for a clean pass"
    );
}

#[tokio::test]
async fn the_two_hop_launder_is_refused_at_link_evolution() {
    // The launder surface: notes authored by a claude-family model (whatever their underlying
    // episode writers were), then linked by the SAME family. The note's own author is claude —
    // the writer-set union must catch it even though no underlying episode writer is claude.
    let memory = memory_with_mode(GuardMode::Refuse);
    let ns = Namespace::Agent("alice".to_string());
    seed_note(&memory, b"laundered-a", &ns, "claude-sonnet-4-6");
    seed_note(&memory, b"laundered-b", &ns, "claude-sonnet-4-6");

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
