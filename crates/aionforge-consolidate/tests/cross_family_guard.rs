//! Driver-level acceptance for the cross-family guard wiring (07 §3, M6.T01): the
//! link-evolution pass refuses (or warns through) a same-family or unverifiable item
//! BEFORE the evolver call, writes the `subliminal_guard_warning` audit row, and
//! leaves clean cross-family work untouched. The substrate-level probe through the
//! engine facade is the M6.T01 acceptance test; these pin the enforcement point
//! itself. The shipped consolidation path is deterministic; the guard is exercised
//! through an in-test inference-evolver double that declares a model family.

use std::future::Future;
use std::sync::Arc;

use aionforge_consolidate::{LinkEvolveConfig, LinkEvolvePass};
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::{EvolvedLink, LinkEvolver, LinkEvolverIdentity};
use aionforge_domain::embedding::Embedding;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::Timestamp;
use aionforge_security::GuardMode;
use aionforge_store::{BoundQuery, MaterializedNote, QueryResult, Store, StoreConfig, Value};

const DIM: usize = 4;

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

/// An evolver that always proposes a link to the first candidate, declaring a
/// chosen family — the in-test stand-in for an inference-backed evolver, so the
/// guard (and only the guard) decides the outcome. Implements [`LinkEvolver`]
/// directly; it never touches a chat/completion client.
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
/// authoring model (the two-hop launder surface). The note is written through the
/// surviving note materializer, and a `Distill`-kind audit anchored on the note id
/// hand-seeds the retained-for-decode tombstone that the note-lineage / writer-family
/// union reads — so the launder coverage stands without the removed distiller.
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
    store
        .seed_notes_for_test(
            &[MaterializedNote {
                note,
                source_facts: Vec::new(),
            }],
            &now(),
        )
        .expect("seed note");

    // Hand-seed the retained `Distill` tombstone, anchored on the note id so the
    // by-subject lineage union decodes the authoring model — what the two-hop launder
    // guard reads. (`AuditKind::Distill` is no longer emitted anywhere; this exercises
    // the decode path that survives.)
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
    store.commit_audit(&audit).expect("seed distill tombstone");
    id
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
async fn an_unverifiable_note_is_refused_fail_closed() {
    let store = store();
    let ns = Namespace::Agent("alice".to_string());
    // Notes with no authoring-model provenance and no underlying episode writers:
    // nobody can vouch for the source family, so the guard refuses fail-closed.
    seed_note(&store, b"unverifiable-a", &ns, "");
    seed_note(&store, b"unverifiable-b", &ns, "");

    let pass = LinkEvolvePass::new(
        FakeEvolver::with_family("mock"),
        LinkEvolveConfig {
            enabled: true,
            ..LinkEvolveConfig::default()
        },
        GuardMode::Refuse,
    );
    let report = pass.evolve_links(&store, &ns, &now()).await.expect("run");

    assert!(report.guard_refused >= 1);
    assert_eq!(report.links_created, 0);
    let payloads = guard_payloads(&store);
    assert!(
        payloads.iter().all(|p| p.contains("unverifiable_writer")),
        "unverifiable, never 'differs': {payloads:?}"
    );
}

#[tokio::test]
async fn warn_mode_proceeds_anyway_but_audits_the_finding() {
    let store = store();
    let ns = Namespace::Agent("alice".to_string());
    seed_note(&store, b"warn-a", &ns, "mock-large");
    seed_note(&store, b"warn-b", &ns, "mock-large");

    let pass = LinkEvolvePass::new(
        FakeEvolver::with_family("mock"),
        LinkEvolveConfig {
            enabled: true,
            ..LinkEvolveConfig::default()
        },
        GuardMode::Warn,
    );
    let report = pass.evolve_links(&store, &ns, &now()).await.expect("run");

    assert_eq!(report.guard_refused, 0, "warn mode refuses nothing");
    assert!(report.links_created >= 1, "the link is still drawn");
    let payloads = guard_payloads(&store);
    assert!(
        payloads.iter().any(|p| p.contains("warned")),
        "the finding is audited as a warning: {payloads:?}"
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
async fn a_refused_source_audits_idempotently_across_reruns() {
    let store = store();
    let ns = Namespace::Agent("alice".to_string());
    seed_note(&store, b"idem-a", &ns, "mock-large");
    seed_note(&store, b"idem-b", &ns, "mock-large");

    let pass = LinkEvolvePass::new(
        FakeEvolver::with_family("mock"),
        LinkEvolveConfig {
            enabled: true,
            ..LinkEvolveConfig::default()
        },
        GuardMode::Refuse,
    );
    pass.evolve_links(&store, &ns, &now()).await.expect("run");
    let after_first = guard_rows(&store);
    pass.evolve_links(
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
