//! Store-level tests for the note-link-evolution write surface (M3.T09): `notes_in_namespace`
//! pools live notes deterministically, `relates_to_links` reads a note's `RELATES_TO` edges with
//! their `EdgeId`s, and `materialize_link_edges` opens/closes bi-temporal links and writes
//! `link_evolve` provenance — in its own transaction, idempotently, off the consolidation cursor.

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::associative::Note;
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::time::Timestamp;
use aionforge_store::{
    BoundQuery, DistilledNoteWrite, EdgeId, LinkEdgeWrite, MaterializedNote, QueryResult, Store,
    StoreConfig, Value,
};

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
        embedding_dimension: 4,
    })
    .expect("open store");
    store
        .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
        .expect("migrate store");
    store
}

fn stats() -> Stats {
    Stats {
        importance: 0.5,
        trust: 0.8,
        last_access: now(),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.1,
        is_pinned: false,
    }
}

fn identity(id: Id, namespace: &Namespace) -> Identity {
    Identity {
        id,
        ingested_at: ts("2026-06-06T09:00:00-05:00[America/Chicago]"),
        namespace: namespace.clone(),
        expired_at: None,
    }
}

/// Seed a `Note` via the distilled-note write path (the public note-write surface), returning its
/// id. The paired `distill` audit is incidental — link tests assert on `link_evolve` audits only.
fn seed_note(store: &Store, seed: &[u8], namespace: &Namespace) -> Id {
    seed_note_with_expiry(store, seed, namespace, None)
}

/// Seed a `Note` whose identity carries `expired_at`, exercising the live-note filter in
/// `notes_in_namespace`.
fn seed_note_with_expiry(
    store: &Store,
    seed: &[u8],
    namespace: &Namespace,
    expired_at: Option<Timestamp>,
) -> Id {
    let id = Id::from_content_hash(seed);
    let mut ident = identity(id, namespace);
    ident.expired_at = expired_at;
    let note = Note {
        identity: ident,
        stats: stats(),
        content: format!("note {}", String::from_utf8_lossy(seed)),
        context: None,
        keywords: Vec::new(),
        embedding: None,
        embedder_model: None,
        derived_from_episode: None,
    };
    let audit = AuditEvent {
        identity: identity(
            Id::from_content_hash(&[seed, b"-seed-audit"].concat()),
            namespace,
        ),
        kind: AuditKind::Distill,
        subject_id: id,
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

fn link_evolve_audit(id_seed: &[u8], source: &Id, namespace: &Namespace) -> AuditEvent {
    AuditEvent {
        identity: identity(Id::from_content_hash(id_seed), namespace),
        kind: AuditKind::LinkEvolve,
        subject_id: *source,
        actor_id: Id::from_content_hash(b"link-evolver/link-evolve-v1"),
        payload: serde_json::json!({
            "outcome": "created",
            "model_family": "claude",
            "endpoint": "https://api.test/v1",
            "seed": 42,
        }),
        signature: String::new(),
        occurred_at: now(),
    }
}

fn create(source: &Id, target: &Id, label: &str, valid_from: Timestamp) -> LinkEdgeWrite {
    LinkEdgeWrite {
        source_id: *source,
        target_id: *target,
        relationship_label: label.to_string(),
        valid_from,
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

fn relates_to_edges(store: &Store) -> u64 {
    count(
        store,
        "MATCH (:Note)-[r:RELATES_TO]->(:Note) RETURN count(r) AS n",
    )
}

fn link_evolve_audit_edges(store: &Store) -> u64 {
    count(
        store,
        "MATCH (a:AuditEvent)-[r:AUDIT]->(:Note) WHERE a.kind = 'link_evolve' RETURN count(r) AS n",
    )
}

fn agent(name: &str) -> Namespace {
    Namespace::Agent(name.to_string())
}

#[test]
fn notes_in_namespace_returns_live_notes_sorted_and_bounded() {
    let store = store();
    let a = agent("alice");
    let b = agent("bob");
    seed_note(&store, b"n3", &a);
    seed_note(&store, b"n1", &a);
    seed_note(&store, b"n2", &a);
    seed_note(&store, b"other", &b);

    let pool = store.notes_in_namespace(&a, 10).expect("pool");
    assert_eq!(pool.len(), 3, "only namespace A's notes");
    let ids: Vec<Id> = pool.iter().map(|n| n.identity.id).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "pool is sorted by id (deterministic)");

    let bounded = store.notes_in_namespace(&a, 2).expect("bounded");
    assert_eq!(bounded.len(), 2, "the limit bounds the pool");

    assert_eq!(store.notes_in_namespace(&b, 10).expect("b").len(), 1);
}

#[test]
fn materialize_link_edges_creates_a_link_idempotently_with_provenance() {
    let store = store();
    let ns = agent("alice");
    let n1 = seed_note(&store, b"n1", &ns);
    let n2 = seed_note(&store, b"n2", &ns);

    let audit = link_evolve_audit(b"audit-create", &n1, &ns);
    store
        .materialize_link_edges(
            &[create(&n1, &n2, "related_to", now())],
            &[],
            std::slice::from_ref(&audit),
            &now(),
        )
        .expect("create link");

    assert_eq!(relates_to_edges(&store), 1, "one RELATES_TO edge");
    let links = store.relates_to_links(&n1).expect("links");
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].target_id, n2);
    assert_eq!(links[0].relationship_label, "related_to");
    assert!(links[0].live, "the new link is current");
    assert_eq!(
        link_evolve_audit_edges(&store),
        1,
        "audit wired to the source note"
    );

    // Replay the same create + audit: value-keyed dedup makes it a no-op.
    store
        .materialize_link_edges(
            &[create(&n1, &n2, "related_to", now())],
            &[],
            std::slice::from_ref(&audit),
            &now(),
        )
        .expect("replay create");
    assert_eq!(relates_to_edges(&store), 1, "replay opens no second edge");
    assert_eq!(
        link_evolve_audit_edges(&store),
        1,
        "replay adds no second audit edge"
    );
}

#[test]
fn revising_a_link_closes_the_old_version_and_opens_the_new() {
    let store = store();
    let ns = agent("alice");
    let n1 = seed_note(&store, b"n1", &ns);
    let n2 = seed_note(&store, b"n2", &ns);

    store
        .materialize_link_edges(
            &[create(&n1, &n2, "related_to", now())],
            &[],
            &[link_evolve_audit(b"audit-create", &n1, &ns)],
            &now(),
        )
        .expect("create link");
    let prior = store.relates_to_links(&n1).expect("links");
    let prior_edge = prior[0].edge_id;

    // Revise the relationship: close the prior version, open a new one with a different label.
    store
        .materialize_link_edges(
            &[create(&n1, &n2, "subsumes", later())],
            &[prior_edge],
            &[link_evolve_audit(b"audit-revise", &n1, &ns)],
            &later(),
        )
        .expect("revise link");

    assert_eq!(
        relates_to_edges(&store),
        2,
        "both the closed and the new version exist"
    );
    let links = store.relates_to_links(&n1).expect("links");
    let live: Vec<&aionforge_store::RelatesToLink> = links.iter().filter(|l| l.live).collect();
    assert_eq!(live.len(), 1, "exactly one current link after revision");
    assert_eq!(
        live[0].relationship_label, "subsumes",
        "the current label is the revised one"
    );
    let closed = links.iter().filter(|l| !l.live).count();
    assert_eq!(
        closed, 1,
        "the prior version is closed, not deleted (non-lossy)"
    );

    // Replaying the close is idempotent (the prior edge is already closed).
    store
        .materialize_link_edges(&[], &[prior_edge], &[], &later())
        .expect("replay close");
    assert_eq!(relates_to_edges(&store), 2, "replay closes nothing new");
}

#[test]
fn link_evolution_never_creates_an_episode_or_cursor() {
    let store = store();
    let ns = agent("alice");
    let n1 = seed_note(&store, b"n1", &ns);
    let n2 = seed_note(&store, b"n2", &ns);
    store
        .materialize_link_edges(
            &[create(&n1, &n2, "related_to", now())],
            &[],
            &[link_evolve_audit(b"audit", &n1, &ns)],
            &now(),
        )
        .expect("create link");

    assert_eq!(
        count(&store, "MATCH (e:Episode) RETURN count(e) AS n"),
        0,
        "no episode"
    );
    assert_eq!(
        count(&store, "MATCH (c:ConsolidationCursor) RETURN count(c) AS n"),
        0,
        "no cursor"
    );
}

#[test]
fn an_empty_link_run_is_a_no_op() {
    let store = store();
    store
        .materialize_link_edges(&[], &[], &[], &now())
        .expect("empty run");
    assert_eq!(relates_to_edges(&store), 0);
}

#[test]
fn notes_in_namespace_honors_zero_and_one_limits() {
    let store = store();
    let ns = agent("alice");
    seed_note(&store, b"n1", &ns);
    seed_note(&store, b"n2", &ns);
    assert!(
        store.notes_in_namespace(&ns, 0).expect("pool").is_empty(),
        "a zero limit yields no candidates"
    );
    assert_eq!(
        store.notes_in_namespace(&ns, 1).expect("pool").len(),
        1,
        "a limit of one yields exactly one candidate"
    );
}

#[test]
fn notes_in_namespace_excludes_expired_notes() {
    let store = store();
    let ns = agent("alice");
    seed_note(&store, b"live", &ns);
    seed_note_with_expiry(&store, b"gone", &ns, Some(later()));
    let pool = store.notes_in_namespace(&ns, 10).expect("pool");
    assert_eq!(pool.len(), 1, "only the live note is pooled");
    assert_eq!(pool[0].content, "note live", "the expired note is dropped");
}

#[test]
fn a_different_label_create_is_refused_until_the_prior_is_closed() {
    let store = store();
    let ns = agent("alice");
    let n1 = seed_note(&store, b"n1", &ns);
    let n2 = seed_note(&store, b"n2", &ns);

    store
        .materialize_link_edges(
            &[create(&n1, &n2, "related_to", now())],
            &[],
            &[link_evolve_audit(b"audit-create", &n1, &ns)],
            &now(),
        )
        .expect("create link");

    // A different-label create on the same live pair, without closing first, is refused: the
    // invariant is one current relationship per ordered pair. It is a skip, not an error.
    store
        .materialize_link_edges(
            &[create(&n1, &n2, "subsumes", later())],
            &[],
            &[link_evolve_audit(b"audit-skip", &n1, &ns)],
            &later(),
        )
        .expect("refused create is not an error");
    assert_eq!(
        relates_to_edges(&store),
        1,
        "no second current edge forks the pair"
    );
    let links = store.relates_to_links(&n1).expect("links");
    let live: Vec<&aionforge_store::RelatesToLink> = links.iter().filter(|l| l.live).collect();
    assert_eq!(live.len(), 1, "still exactly one current link");
    assert_eq!(
        live[0].relationship_label, "related_to",
        "the original label is untouched"
    );

    // Staged correctly — close the prior version and create the new one in the same call — the
    // relabel lands (the close runs before the create, so the pair is free when the create probes).
    let prior_edge = live[0].edge_id;
    store
        .materialize_link_edges(
            &[create(&n1, &n2, "subsumes", later())],
            &[prior_edge],
            &[link_evolve_audit(b"audit-relabel", &n1, &ns)],
            &later(),
        )
        .expect("staged relabel");
    let links = store.relates_to_links(&n1).expect("links");
    let live: Vec<&aionforge_store::RelatesToLink> = links.iter().filter(|l| l.live).collect();
    assert_eq!(live.len(), 1, "still one current link after the relabel");
    assert_eq!(live[0].relationship_label, "subsumes", "now relabeled");
    assert_eq!(
        links.iter().filter(|l| !l.live).count(),
        1,
        "the prior version is closed, not deleted"
    );
}

#[test]
fn closing_a_non_existent_edge_is_a_no_op() {
    let store = store();
    let ns = agent("alice");
    let n1 = seed_note(&store, b"n1", &ns);
    let n2 = seed_note(&store, b"n2", &ns);

    // A bogus EdgeId in the closes list is skipped, not fatal — and it does not block a create
    // batched alongside it (the close loop must distinguish "absent" from "open").
    let bogus = EdgeId::new(9_999_999);
    store
        .materialize_link_edges(
            &[create(&n1, &n2, "related_to", now())],
            &[bogus],
            &[link_evolve_audit(b"audit", &n1, &ns)],
            &now(),
        )
        .expect("a non-existent close is skipped, not fatal");
    assert_eq!(
        relates_to_edges(&store),
        1,
        "the batched create still lands"
    );
    let links = store.relates_to_links(&n1).expect("links");
    assert_eq!(links.len(), 1);
    assert!(links[0].live, "the create is current");
}
