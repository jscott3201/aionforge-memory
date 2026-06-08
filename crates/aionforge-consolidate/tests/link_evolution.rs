//! End-to-end tests for the off-cursor link evolver (M3.T09): the [`LinkEvolvePass`] pools a
//! namespace's live notes, offers each source its nearest embedding neighbors, asks the evolver
//! which relationships hold, and writes bi-temporal `RELATES_TO` edges with `link_evolve`
//! provenance — entirely off the consolidation cursor.
//!
//! Hermetic: notes are seeded through the public distilled-note write path with explicit
//! embeddings, the deterministic [`RuleLinkEvolver`] drives the proximity path, and a small
//! `ScriptedEvolver` drives the create / revise / decline / cap / vocabulary paths with canned
//! proposals that always point at a real offered candidate.

use std::convert::Infallible;
use std::future::Future;

use aionforge_consolidate::{LinkEvolveConfig, LinkEvolvePass, RuleLinkEvolver};
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::contracts::{EvolvedLink, LinkEvolver, LinkEvolverIdentity};
use aionforge_domain::embedding::Embedding;
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::Timestamp;
use aionforge_store::{
    BoundQuery, DistilledNoteWrite, MaterializedNote, QueryResult, Store, StoreConfig, Value,
};

const DIM: usize = 4;

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

fn now() -> Timestamp {
    ts("2026-06-06T12:00:00-05:00[America/Chicago]")
}

fn later() -> Timestamp {
    ts("2026-06-06T13:00:00-05:00[America/Chicago]")
}

fn store() -> Store {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: DIM as u32,
    })
    .expect("open store");
    store
        .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
        .expect("migrate store");
    store
}

fn agent(name: &str) -> Namespace {
    Namespace::Agent(name.to_string())
}

fn stats() -> Stats {
    Stats {
        importance: 0.5,
        trust: 0.8,
        last_access: now(),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.0,
        is_pinned: false,
    }
}

/// Seed a `Note` with an explicit embedding through the distilled-note write path, returning its id.
fn seed_note(store: &Store, seed: &[u8], ns: &Namespace, embedding: [f32; DIM]) -> Id {
    let id = Id::from_content_hash(seed);
    let note = Note {
        identity: Identity {
            id: id.clone(),
            ingested_at: now(),
            namespace: ns.clone(),
            expired_at: None,
        },
        stats: stats(),
        content: format!("note {}", String::from_utf8_lossy(seed)),
        context: None,
        keywords: Vec::new(),
        embedding: Some(Embedding::new(embedding.to_vec()).expect("valid embedding")),
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
        subject_id: id.clone(),
        actor_id: Id::from_content_hash(b"seed"),
        payload: serde_json::json!({"outcome": "written"}),
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

fn relates_to_edges(store: &Store) -> u64 {
    count(
        store,
        "MATCH (:Note)-[r:RELATES_TO]->(:Note) RETURN count(r) AS n",
    )
}

fn link_evolve_audits(store: &Store) -> u64 {
    count(
        store,
        "MATCH (a:AuditEvent)-[r:AUDIT]->(:Note) WHERE a.kind = 'link_evolve' RETURN count(r) AS n",
    )
}

/// A canned evolver: it proposes one link from the source to its first offered candidate with a
/// fixed label and confidence, or declines. Pointing at `candidates[0]` keeps the target a real
/// offered candidate so the driver's membership check passes.
#[derive(Clone)]
struct ScriptedEvolver {
    label: String,
    confidence: f64,
    decline: bool,
    identity: LinkEvolverIdentity,
}

impl ScriptedEvolver {
    fn proposing(label: &str, confidence: f64) -> Self {
        Self {
            label: label.to_string(),
            confidence,
            decline: false,
            identity: LinkEvolverIdentity {
                model_family: Some("mock".to_string()),
                model_version: Some("1".to_string()),
                rule_version: "scripted-v1".to_string(),
            },
        }
    }

    fn declining() -> Self {
        Self {
            decline: true,
            ..Self::proposing("related_to", 1.0)
        }
    }
}

impl LinkEvolver for ScriptedEvolver {
    type Error = Infallible;

    fn evolve(
        &self,
        _source: &Note,
        candidates: &[Note],
    ) -> impl Future<Output = Result<Option<Vec<EvolvedLink>>, Self::Error>> + Send {
        let out = if self.decline {
            None
        } else {
            candidates.first().map(|c| {
                vec![EvolvedLink {
                    target_id: c.identity.id.clone(),
                    relationship_label: self.label.clone(),
                    confidence: self.confidence,
                }]
            })
        };
        async move { Ok(out) }
    }

    fn identity(&self) -> &LinkEvolverIdentity {
        &self.identity
    }
}

/// A config that runs (enabled) with a permissive floor, so a test only exercises the path it sets.
fn enabled_config() -> LinkEvolveConfig {
    LinkEvolveConfig {
        enabled: true,
        confidence_floor: 0.5,
        ..LinkEvolveConfig::default()
    }
}

#[tokio::test]
async fn disabled_is_a_no_op() {
    let store = store();
    let ns = agent("alice");
    seed_note(&store, b"a", &ns, [1.0, 0.0, 0.0, 0.0]);
    seed_note(&store, b"b", &ns, [1.0, 0.0, 0.0, 0.0]);
    let pass = LinkEvolvePass::new(
        RuleLinkEvolver::with_default_rules(),
        LinkEvolveConfig::default(),
    );
    let report = pass.evolve_links(&store, &ns, &now()).await.expect("run");
    assert_eq!(report, aionforge_consolidate::LinkEvolveReport::default());
    assert_eq!(relates_to_edges(&store), 0, "nothing runs while disabled");
}

#[tokio::test]
async fn the_rule_evolver_links_nearby_notes_and_never_touches_the_cursor() {
    let store = store();
    let ns = agent("alice");
    // Two identical vectors (cosine 1.0) and one orthogonal (cosine 0.0, dropped by the floor).
    seed_note(&store, b"a", &ns, [1.0, 0.0, 0.0, 0.0]);
    seed_note(&store, b"b", &ns, [1.0, 0.0, 0.0, 0.0]);
    seed_note(&store, b"c", &ns, [0.0, 1.0, 0.0, 0.0]);

    let pass = LinkEvolvePass::new(RuleLinkEvolver::with_default_rules(), enabled_config());
    let report = pass.evolve_links(&store, &ns, &now()).await.expect("run");

    // a↔b are mutually nearest (each links to the other); c relates to neither above the floor.
    assert_eq!(report.links_created, 2, "a→b and b→a");
    assert_eq!(report.links_revised, 0);
    assert_eq!(relates_to_edges(&store), 2);
    assert!(
        link_evolve_audits(&store) >= 2,
        "each create wired an audit"
    );

    // Off-cursor: no episode, no consolidation cursor was created.
    assert_eq!(count(&store, "MATCH (e:Episode) RETURN count(e) AS n"), 0);
    assert_eq!(
        count(&store, "MATCH (c:ConsolidationCursor) RETURN count(c) AS n"),
        0
    );
}

#[tokio::test]
async fn a_second_identical_run_creates_nothing_new() {
    let store = store();
    let ns = agent("alice");
    seed_note(&store, b"a", &ns, [1.0, 0.0, 0.0, 0.0]);
    seed_note(&store, b"b", &ns, [1.0, 0.0, 0.0, 0.0]);
    let pass = LinkEvolvePass::new(RuleLinkEvolver::with_default_rules(), enabled_config());
    pass.evolve_links(&store, &ns, &now()).await.expect("run 1");
    let after_first = relates_to_edges(&store);
    let report = pass
        .evolve_links(&store, &ns, &later())
        .await
        .expect("run 2");
    assert_eq!(report.links_created, 0, "same-label live links are no-ops");
    assert_eq!(relates_to_edges(&store), after_first, "no new edges");
}

#[tokio::test]
async fn a_relabel_revises_the_live_link() {
    let store = store();
    let ns = agent("alice");
    seed_note(&store, b"a", &ns, [1.0, 0.0, 0.0, 0.0]);
    seed_note(&store, b"b", &ns, [0.9, 0.1, 0.0, 0.0]);

    // First call: create a→b related_to.
    let create_pass = LinkEvolvePass::new(
        ScriptedEvolver::proposing("related_to", 0.9),
        enabled_config(),
    );
    let r1 = create_pass
        .evolve_links(&store, &ns, &now())
        .await
        .expect("create");
    assert_eq!(
        r1.links_created, 2,
        "a→b and b→a (the scripted evolver links each source's first candidate)"
    );

    // Second call: the same pairs now propose `subsumes` — a relabel closes the old and opens new.
    let revise_pass = LinkEvolvePass::new(
        ScriptedEvolver::proposing("subsumes", 0.9),
        enabled_config(),
    );
    let r2 = revise_pass
        .evolve_links(&store, &ns, &later())
        .await
        .expect("revise");
    assert_eq!(r2.links_revised, 2, "both directions relabel");
    assert_eq!(r2.links_created, 0);

    // Two live `subsumes` and two closed `related_to`: history preserved, one current per pair.
    let live = count(
        &store,
        "MATCH (:Note)-[r:RELATES_TO]->(:Note) WHERE r.relationship_label = 'subsumes' AND r.valid_to IS NULL RETURN count(r) AS n",
    );
    let closed = count(
        &store,
        "MATCH (:Note)-[r:RELATES_TO]->(:Note) WHERE r.valid_to IS NOT NULL RETURN count(r) AS n",
    );
    assert_eq!(live, 2, "the revised label is current");
    assert_eq!(closed, 2, "the prior versions are closed, not deleted");
}

#[tokio::test]
async fn a_declined_call_writes_an_audit_and_no_edge() {
    let store = store();
    let ns = agent("alice");
    seed_note(&store, b"a", &ns, [1.0, 0.0, 0.0, 0.0]);
    seed_note(&store, b"b", &ns, [1.0, 0.0, 0.0, 0.0]);
    let pass = LinkEvolvePass::new(ScriptedEvolver::declining(), enabled_config());
    let report = pass.evolve_links(&store, &ns, &now()).await.expect("run");
    assert_eq!(report.links_created, 0);
    assert!(report.declined >= 1, "the declines are recorded");
    assert_eq!(relates_to_edges(&store), 0, "a declined call draws no edge");
    assert!(link_evolve_audits(&store) >= 1, "the decline is audited");
}

#[tokio::test]
async fn an_out_of_vocabulary_label_is_dropped() {
    let store = store();
    let ns = agent("alice");
    seed_note(&store, b"a", &ns, [1.0, 0.0, 0.0, 0.0]);
    seed_note(&store, b"b", &ns, [1.0, 0.0, 0.0, 0.0]);
    let pass = LinkEvolvePass::new(ScriptedEvolver::proposing("owns", 0.9), enabled_config());
    let report = pass.evolve_links(&store, &ns, &now()).await.expect("run");
    assert_eq!(report.links_created, 0, "a free-text label is refused");
    assert_eq!(relates_to_edges(&store), 0);
}

#[tokio::test]
async fn a_low_confidence_proposal_is_dropped_by_the_floor() {
    let store = store();
    let ns = agent("alice");
    seed_note(&store, b"a", &ns, [1.0, 0.0, 0.0, 0.0]);
    seed_note(&store, b"b", &ns, [1.0, 0.0, 0.0, 0.0]);
    // Below the floor (0.5) — dropped before any edge decision.
    let pass = LinkEvolvePass::new(
        ScriptedEvolver::proposing("related_to", 0.2),
        enabled_config(),
    );
    let report = pass.evolve_links(&store, &ns, &now()).await.expect("run");
    assert_eq!(report.links_created, 0, "sub-floor confidence is dropped");
    assert_eq!(relates_to_edges(&store), 0);
}

#[tokio::test]
async fn the_per_run_create_cap_bounds_the_writes() {
    let store = store();
    let ns = agent("alice");
    // Four mutually-near notes; the scripted evolver would link each source's first candidate.
    for tag in [b"a".as_slice(), b"b", b"c", b"d"] {
        seed_note(&store, tag, &ns, [1.0, 0.0, 0.0, 0.0]);
    }
    let config = LinkEvolveConfig {
        max_links_created_per_run: 1,
        ..enabled_config()
    };
    let pass = LinkEvolvePass::new(ScriptedEvolver::proposing("related_to", 0.9), config);
    let report = pass.evolve_links(&store, &ns, &now()).await.expect("run");
    assert_eq!(report.links_created, 1, "the per-run cap holds");
    assert_eq!(relates_to_edges(&store), 1);
}

#[tokio::test]
async fn the_per_pair_revision_cap_stops_churn() {
    let store = store();
    let ns = agent("alice");
    seed_note(&store, b"a", &ns, [1.0, 0.0, 0.0, 0.0]);
    seed_note(&store, b"b", &ns, [0.9, 0.1, 0.0, 0.0]);
    let config = LinkEvolveConfig {
        max_revisions_per_link: 1,
        max_candidates_per_note: 1,
        ..enabled_config()
    };
    // related_to → subsumes (revision 1, allowed) → elaborates (revision 2, capped).
    for label in ["related_to", "subsumes", "elaborates"] {
        let pass = LinkEvolvePass::new(ScriptedEvolver::proposing(label, 0.9), config.clone());
        pass.evolve_links(&store, &ns, &later()).await.expect("run");
    }
    // One revision allowed per pair: each direction (a→b and b→a) relabeled once, so two
    // `related_to` versions are closed; the second relabel (to `elaborates`) was blocked by the cap.
    let closed_related = count(
        &store,
        "MATCH (:Note)-[r:RELATES_TO]->(:Note) WHERE r.relationship_label = 'related_to' AND r.valid_to IS NOT NULL RETURN count(r) AS n",
    );
    assert_eq!(closed_related, 2, "each direction relabeled exactly once");
    let live_subsumes = count(
        &store,
        "MATCH (:Note)-[r:RELATES_TO]->(:Note) WHERE r.relationship_label = 'subsumes' AND r.valid_to IS NULL RETURN count(r) AS n",
    );
    assert_eq!(
        live_subsumes, 2,
        "subsumes is current after the one allowed revision"
    );
    let elaborates = count(
        &store,
        "MATCH (:Note)-[r:RELATES_TO]->(:Note) WHERE r.relationship_label = 'elaborates' RETURN count(r) AS n",
    );
    assert_eq!(elaborates, 0, "the capped third label never landed");
}
