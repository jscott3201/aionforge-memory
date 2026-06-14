//! End-to-end tests for the off-cursor link evolver (M3.T09): the [`LinkEvolvePass`] pools a
//! namespace's live notes, offers each source its nearest embedding neighbors, asks the evolver
//! which relationships hold, and writes bi-temporal `RELATES_TO` edges with `link_evolve`
//! provenance — entirely off the consolidation cursor.
//!
//! Hermetic: notes are seeded through the store's note materializer with explicit embeddings
//! (plus a hand-seeded `Distill` tombstone recording each note's writer family for the
//! cross-family guard), the deterministic [`RuleLinkEvolver`] drives the proximity path, and a
//! small `MockEvolver` drives the create / revise / decline / empty / dedup / membership / cap /
//! vocabulary paths with canned proposals. No chat/completion client is involved.

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
use aionforge_security::GuardMode;
use aionforge_store::{BoundQuery, MaterializedNote, QueryResult, Store, StoreConfig, Value};

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
    seed_note_opt(store, seed, ns, Some(embedding))
}

/// Seed a `Note` with no embedding (the link evolver must exclude it from the candidate pool).
fn seed_bare_note(store: &Store, seed: &[u8], ns: &Namespace) -> Id {
    seed_note_opt(store, seed, ns, None)
}

fn seed_note_opt(store: &Store, seed: &[u8], ns: &Namespace, embedding: Option<[f32; DIM]>) -> Id {
    let id = Id::from_content_hash(seed);
    let note = Note {
        identity: Identity {
            id,
            ingested_at: now(),
            namespace: ns.clone(),
            expired_at: None,
        },
        stats: stats(),
        content: format!("note {}", String::from_utf8_lossy(seed)),
        context: None,
        keywords: Vec::new(),
        embedding: embedding.map(|e| Embedding::new(e.to_vec()).expect("valid embedding")),
        embedder_model: None,
        derived_from_episode: None,
    };
    // Write the note through the surviving note materializer.
    store
        .seed_notes_for_test(
            &[MaterializedNote {
                note,
                source_facts: Vec::new(),
            }],
            &now(),
        )
        .expect("seed note");

    // Hand-seed a retained `Distill` tombstone anchored on the note id; the recorded model
    // doubles as the note's writer family for the cross-family guard (07 §3). "writer-fake"
    // differs from MockEvolver's "mock", so a clean cross-family pass is not refused. The
    // `Distill` audit kind is no longer emitted by any path — this exercises the surviving
    // by-subject lineage decode the guard reads.
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
        payload: serde_json::json!({"outcome": "written", "model_family": "writer-fake"}),
        signature: String::new(),
        occurred_at: now(),
    };
    store.commit_audit(&audit).expect("seed distill tombstone");
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

/// How a mock proposal names its target: by position in the offered candidate list (the common,
/// valid path), or by an explicit id — which a test can deliberately set to a note that was *not*
/// offered, to exercise the driver's membership guard.
#[derive(Clone)]
enum Target {
    Nth(usize),
    Explicit(Id),
}

/// A flexible mock evolver: it returns a fixed outcome regardless of input — decline, or a list of
/// proposals resolved against the offered candidates — so a test can drive any driver path
/// (decline, empty, single, same-target duplicates, or a non-offered target).
#[derive(Clone)]
struct MockEvolver {
    outcome: Outcome,
    identity: LinkEvolverIdentity,
}

#[derive(Clone)]
enum Outcome {
    Decline,
    /// Proposals as `(target, label, confidence)`; an empty list is "ran, drew nothing".
    Propose(Vec<(Target, String, f64)>),
}

impl MockEvolver {
    fn new(outcome: Outcome) -> Self {
        Self {
            outcome,
            identity: LinkEvolverIdentity {
                model_family: Some("mock".to_string()),
                model_version: Some("1".to_string()),
                rule_version: "mock-v1".to_string(),
            },
        }
    }

    /// One proposal to the first offered candidate — the common valid path.
    fn proposing(label: &str, confidence: f64) -> Self {
        Self::new(Outcome::Propose(vec![(
            Target::Nth(0),
            label.to_string(),
            confidence,
        )]))
    }

    fn declining() -> Self {
        Self::new(Outcome::Decline)
    }

    /// Runs but proposes no relationship (distinct from a decline).
    fn drawing_nothing() -> Self {
        Self::new(Outcome::Propose(Vec::new()))
    }
}

impl LinkEvolver for MockEvolver {
    type Error = Infallible;

    fn evolve(
        &self,
        _source: &Note,
        candidates: &[Note],
    ) -> impl Future<Output = Result<Option<Vec<EvolvedLink>>, Self::Error>> + Send {
        let out = match &self.outcome {
            Outcome::Decline => None,
            Outcome::Propose(specs) => {
                let mut links = Vec::new();
                for (target, label, confidence) in specs {
                    let target_id = match target {
                        Target::Nth(n) => match candidates.get(*n) {
                            Some(c) => c.identity.id,
                            None => continue,
                        },
                        Target::Explicit(id) => *id,
                    };
                    links.push(EvolvedLink {
                        target_id,
                        relationship_label: label.clone(),
                        confidence: *confidence,
                    });
                }
                Some(links)
            }
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
        GuardMode::default(),
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

    let pass = LinkEvolvePass::new(
        RuleLinkEvolver::with_default_rules(),
        enabled_config(),
        GuardMode::default(),
    );
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
    let pass = LinkEvolvePass::new(
        RuleLinkEvolver::with_default_rules(),
        enabled_config(),
        GuardMode::default(),
    );
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
        MockEvolver::proposing("related_to", 0.9),
        enabled_config(),
        GuardMode::default(),
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
        MockEvolver::proposing("subsumes", 0.9),
        enabled_config(),
        GuardMode::default(),
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
    let pass = LinkEvolvePass::new(
        MockEvolver::declining(),
        enabled_config(),
        GuardMode::default(),
    );
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
    let pass = LinkEvolvePass::new(
        MockEvolver::proposing("owns", 0.9),
        enabled_config(),
        GuardMode::default(),
    );
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
        MockEvolver::proposing("related_to", 0.2),
        enabled_config(),
        GuardMode::default(),
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
    let pass = LinkEvolvePass::new(
        MockEvolver::proposing("related_to", 0.9),
        config,
        GuardMode::default(),
    );
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
        let pass = LinkEvolvePass::new(
            MockEvolver::proposing(label, 0.9),
            config.clone(),
            GuardMode::default(),
        );
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

#[tokio::test]
async fn an_empty_proposal_set_draws_nothing_and_is_not_a_decline() {
    let store = store();
    let ns = agent("alice");
    seed_note(&store, b"a", &ns, [1.0, 0.0, 0.0, 0.0]);
    seed_note(&store, b"b", &ns, [1.0, 0.0, 0.0, 0.0]);
    let pass = LinkEvolvePass::new(
        MockEvolver::drawing_nothing(),
        enabled_config(),
        GuardMode::default(),
    );
    let report = pass.evolve_links(&store, &ns, &now()).await.expect("run");
    // The evolver ran on both sources but drew no relationship — distinct from a decline.
    assert_eq!(report.notes_seen, 2, "both sources were consulted");
    assert_eq!(
        report.declined, 0,
        "running and drawing nothing is not a decline"
    );
    assert_eq!(report.links_created, 0);
    assert_eq!(report.links_revised, 0);
    assert_eq!(relates_to_edges(&store), 0, "no edge");
    // The audit trail stays proportional to changes: an empty-but-ran call writes no audit.
    assert_eq!(
        link_evolve_audits(&store),
        0,
        "no material outcome, no audit"
    );
}

#[tokio::test]
async fn the_highest_confidence_proposal_per_target_wins() {
    let store = store();
    let ns = agent("alice");
    seed_note(&store, b"a", &ns, [1.0, 0.0, 0.0, 0.0]);
    seed_note(&store, b"b", &ns, [1.0, 0.0, 0.0, 0.0]);
    // Two proposals to the same target (the sole candidate): the higher-confidence label wins.
    let evolver = MockEvolver::new(Outcome::Propose(vec![
        (Target::Nth(0), "related_to".to_string(), 0.55),
        (Target::Nth(0), "subsumes".to_string(), 0.95),
    ]));
    let pass = LinkEvolvePass::new(evolver, enabled_config(), GuardMode::default());
    pass.evolve_links(&store, &ns, &now()).await.expect("run");
    let live_subsumes = count(
        &store,
        "MATCH (:Note)-[r:RELATES_TO]->(:Note) WHERE r.relationship_label = 'subsumes' AND r.valid_to IS NULL RETURN count(r) AS n",
    );
    let related = count(
        &store,
        "MATCH (:Note)-[r:RELATES_TO]->(:Note) WHERE r.relationship_label = 'related_to' RETURN count(r) AS n",
    );
    assert_eq!(
        live_subsumes, 2,
        "the higher-confidence label is the one drawn (a→b, b→a)"
    );
    assert_eq!(related, 0, "the lower-confidence duplicate is dropped");
}

#[tokio::test]
async fn a_proposal_for_a_non_offered_note_is_dropped() {
    let store = store();
    let ns = agent("alice");
    seed_note(&store, b"a", &ns, [1.0, 0.0, 0.0, 0.0]);
    seed_note(&store, b"b", &ns, [1.0, 0.0, 0.0, 0.0]);
    // Point every proposal at an id that is a valid hash but was never seeded or offered.
    let bogus = Id::from_content_hash(b"not-seeded-anywhere");
    let evolver = MockEvolver::new(Outcome::Propose(vec![(
        Target::Explicit(bogus),
        "related_to".to_string(),
        0.99,
    )]));
    let pass = LinkEvolvePass::new(evolver, enabled_config(), GuardMode::default());
    let report = pass.evolve_links(&store, &ns, &now()).await.expect("run");
    assert_eq!(report.links_created, 0, "a non-offered target is refused");
    assert_eq!(
        relates_to_edges(&store),
        0,
        "no edge to a note that was not a candidate"
    );
}

#[tokio::test]
async fn link_evolution_is_namespace_scoped() {
    let store = store();
    let alice = agent("alice");
    let bob = agent("bob");
    let a1 = seed_note(&store, b"a1", &alice, [1.0, 0.0, 0.0, 0.0]);
    let a2 = seed_note(&store, b"a2", &alice, [1.0, 0.0, 0.0, 0.0]);
    let b1 = seed_note(&store, b"b1", &bob, [1.0, 0.0, 0.0, 0.0]);
    seed_note(&store, b"b2", &bob, [1.0, 0.0, 0.0, 0.0]);

    // Run only on alice's namespace.
    let pass = LinkEvolvePass::new(
        RuleLinkEvolver::with_default_rules(),
        enabled_config(),
        GuardMode::default(),
    );
    pass.evolve_links(&store, &alice, &now())
        .await
        .expect("run");

    assert_eq!(
        relates_to_edges(&store),
        2,
        "only alice's pair is linked (a1↔a2)"
    );
    // a1 links only to a2 — never across the namespace to bob's notes.
    let a1_links = store.relates_to_links(&a1).expect("a1 links");
    assert_eq!(a1_links.len(), 1);
    assert_eq!(
        a1_links[0].target_id, a2,
        "a1 relates only within its namespace"
    );
    // bob's notes were never offered as candidates and got no links.
    assert!(
        store.relates_to_links(&b1).expect("b1 links").is_empty(),
        "bob's namespace is untouched"
    );
}

#[tokio::test]
async fn embeddingless_notes_are_excluded_from_the_pool() {
    let store = store();
    let ns = agent("alice");
    seed_note(&store, b"a", &ns, [1.0, 0.0, 0.0, 0.0]);
    seed_note(&store, b"b", &ns, [1.0, 0.0, 0.0, 0.0]);
    let c = seed_bare_note(&store, b"c", &ns); // no embedding

    let pass = LinkEvolvePass::new(
        RuleLinkEvolver::with_default_rules(),
        enabled_config(),
        GuardMode::default(),
    );
    pass.evolve_links(&store, &ns, &now()).await.expect("run");

    assert_eq!(
        relates_to_edges(&store),
        2,
        "only the embedded pair links (a↔b)"
    );
    assert!(
        store.relates_to_links(&c).expect("c links").is_empty(),
        "the embeddingless note is neither a source nor a candidate"
    );
}

#[tokio::test]
async fn a_source_with_no_embedded_neighbors_is_skipped() {
    let store = store();
    let ns = agent("alice");
    seed_note(&store, b"a", &ns, [1.0, 0.0, 0.0, 0.0]);
    seed_bare_note(&store, b"b", &ns); // the only other note has no embedding

    let pass = LinkEvolvePass::new(
        RuleLinkEvolver::with_default_rules(),
        enabled_config(),
        GuardMode::default(),
    );
    let report = pass.evolve_links(&store, &ns, &now()).await.expect("run");
    // `a` has an embedding but no embedded neighbor to be offered, so it is never consulted.
    assert_eq!(report.notes_seen, 0, "no source had any candidate");
    assert_eq!(relates_to_edges(&store), 0);
}

#[tokio::test]
async fn candidate_tie_breaks_are_deterministic_by_id() {
    let store = store();
    let ns = agent("alice");
    let s = seed_note(&store, b"s", &ns, [1.0, 0.0, 0.0, 0.0]);
    let x = seed_note(&store, b"x", &ns, [1.0, 0.0, 0.0, 0.0]);
    let y = seed_note(&store, b"y", &ns, [1.0, 0.0, 0.0, 0.0]);
    // x and y are equidistant from s (identical vectors); with only one candidate slot, the tie
    // must break by the lexicographically smaller id — deterministically, every run.
    let smaller = if x < y { x } else { y };
    let config = LinkEvolveConfig {
        max_candidates_per_note: 1,
        ..enabled_config()
    };
    let pass = LinkEvolvePass::new(
        RuleLinkEvolver::with_default_rules(),
        config,
        GuardMode::default(),
    );
    pass.evolve_links(&store, &ns, &now()).await.expect("run");
    let s_links = store.relates_to_links(&s).expect("s links");
    assert_eq!(s_links.len(), 1, "exactly one candidate slot was filled");
    assert_eq!(
        s_links[0].target_id, smaller,
        "the smaller-id neighbor wins the tie deterministically"
    );
}
