//! Store-level tests for the active-forgetting read surfaces (05 §2, M5.T02):
//! `forgettable_candidates` pages the sweep-scoped population (Episode + Fact,
//! `expired_at IS NULL`, keyset over `(label, id)`), and `has_protecting_reference`
//! answers the "unreferenced" axis from live incoming edges, never the
//! `referenced_count` cache.

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::semantic::{Fact, FactStatus};
use aionforge_domain::time::Timestamp;
use aionforge_domain::value::ObjectValue;
use aionforge_store::{BoundQuery, ForgetCursor, Store, StoreConfig, Value};

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

fn now() -> Timestamp {
    ts("2026-06-06T12:00:00-05:00[America/Chicago]")
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

fn identity(expired: bool) -> Identity {
    Identity {
        id: Id::generate(),
        ingested_at: ts("2026-06-01T09:00:00-05:00[America/Chicago]"),
        namespace: Namespace::Global,
        expired_at: expired.then(now),
    }
}

fn stats() -> Stats {
    Stats {
        importance: 0.04,
        trust: 0.2,
        last_access: ts("2026-06-01T09:00:00-05:00[America/Chicago]"),
        access_count_recent: 0,
        referenced_count: 7, // deliberately wrong vs the graph: the probe must not read it
        surprise: 0.1,
        is_pinned: false,
    }
}

fn episode(expired: bool) -> Episode {
    let content = format!("episode {}", Id::generate());
    Episode {
        identity: identity(expired),
        stats: stats(),
        content: content.clone(),
        role: Role::User,
        captured_at: now(),
        agent_id: Id::from_content_hash(b"test-agent"),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    }
}

fn fact(expired: bool) -> Fact {
    Fact {
        identity: identity(expired),
        stats: stats(),
        subject_id: Id::from_content_hash(b"subject"),
        predicate: "tests".to_string(),
        object: ObjectValue::Text("forgetting".to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: "tests forgetting".to_string(),
        embedding: None,
        embedder_model: None,
        extraction: None,
    }
}

fn support_edge(store: &Store, from: &Id, to: &Id) {
    let query = BoundQuery::new(
        "MATCH (a:Fact {id: $from}), (b:Fact {id: $to}) \
         INSERT (a)-[:SUPPORTS {weight: $weight}]->(b)",
    )
    .bind_uuid("from", from)
    .unwrap()
    .bind_uuid("to", to)
    .unwrap()
    .bind("weight", Value::Float(1.0))
    .unwrap();
    store.execute(&query).expect("insert SUPPORTS edge");
}

#[test]
fn candidate_pages_scope_filter_and_order() {
    let store = store();
    let live_episode = episode(false);
    let dead_episode = episode(true);
    let live_facts = [fact(false), fact(false)];
    let dead_fact = fact(true);
    store.insert_episode(&live_episode).expect("insert");
    store.insert_episode(&dead_episode).expect("insert");
    for f in &live_facts {
        store.insert_fact(f).expect("insert");
    }
    store.insert_fact(&dead_fact).expect("insert");

    let page = store.forgettable_candidates(None, 50).expect("page");
    assert_eq!(page.candidates.len(), 3, "expired nodes never enter a page");
    assert!(page.next.is_none(), "an unfilled page ends the scan");

    // Scan order: Episode label first, then Facts in byte-ordered id order.
    assert_eq!(page.candidates[0].label, "Episode");
    assert_eq!(page.candidates[0].identity.id, live_episode.identity.id);
    let mut fact_ids: Vec<Id> = live_facts.iter().map(|f| f.identity.id).collect();
    fact_ids.sort();
    let returned: Vec<Id> = page.candidates[1..].iter().map(|c| c.identity.id).collect();
    assert_eq!(returned, fact_ids, "facts in id order");

    // The blocks the axes read round-trip.
    let candidate = &page.candidates[0];
    assert!((candidate.stats.importance - 0.04).abs() < 1e-12);
    assert!((candidate.stats.trust - 0.2).abs() < 1e-12);
    assert!(!candidate.stats.is_pinned);
    assert!(candidate.identity.expired_at.is_none());
}

#[test]
fn pagination_resumes_exactly_where_the_scan_left_off() {
    let store = store();
    for _ in 0..2 {
        store.insert_episode(&episode(false)).expect("insert");
    }
    for _ in 0..3 {
        store.insert_fact(&fact(false)).expect("insert");
    }

    let full: Vec<Id> = store
        .forgettable_candidates(None, 50)
        .expect("full scan")
        .candidates
        .iter()
        .map(|c| c.identity.id)
        .collect();
    assert_eq!(full.len(), 5);

    // Walk the same population two at a time; the concatenation must equal the full
    // scan, label boundary included.
    let mut walked = Vec::new();
    let mut cursor: Option<ForgetCursor> = None;
    loop {
        let page = store
            .forgettable_candidates(cursor.as_ref(), 2)
            .expect("page");
        walked.extend(page.candidates.iter().map(|c| c.identity.id));
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(walked, full, "paged walk visits exactly the full scan");
}

#[test]
fn a_cursor_from_outside_the_scan_order_is_rejected() {
    let store = store();
    let bogus = ForgetCursor {
        label: "CoreBlock".to_string(),
        id: Id::generate(),
    };
    let error = store
        .forgettable_candidates(Some(&bogus), 10)
        .expect_err("a label outside the scan order is not a resumable position");
    assert!(error.to_string().contains("CoreBlock"), "{error}");
}

#[test]
fn the_reference_probe_reads_live_edges_not_the_count_cache() {
    let store = store();
    let supported = fact(false);
    let supporter = fact(false);
    store.insert_fact(&supported).expect("insert");
    store.insert_fact(&supporter).expect("insert");
    support_edge(&store, &supporter.identity.id, &supported.identity.id);

    let page = store.forgettable_candidates(None, 50).expect("page");
    let node_of = |id: &Id| {
        page.candidates
            .iter()
            .find(|c| c.identity.id == *id)
            .expect("candidate present")
            .node
    };

    // Incoming SUPPORTS protects the target; the source has only an outgoing edge.
    assert!(
        store
            .has_protecting_reference(node_of(&supported.identity.id), &["SUPPORTS"])
            .expect("probe")
    );
    assert!(
        !store
            .has_protecting_reference(node_of(&supporter.identity.id), &["SUPPORTS"])
            .expect("probe"),
        "an outgoing edge does not protect the source"
    );
    // A label outside the allowlist never protects — even though referenced_count is
    // seeded non-zero on every fixture, the cache is never consulted.
    assert!(
        !store
            .has_protecting_reference(node_of(&supported.identity.id), &["DEPENDS_ON"])
            .expect("probe")
    );
    // An isolated node is unprotected.
    let lonely = fact(false);
    store.insert_fact(&lonely).expect("insert");
    let page = store.forgettable_candidates(None, 50).expect("page");
    let lonely_node = page
        .candidates
        .iter()
        .find(|c| c.identity.id == lonely.identity.id)
        .expect("present")
        .node;
    assert!(
        !store
            .has_protecting_reference(lonely_node, &["SUPPORTS", "DEPENDS_ON", "MENTIONS"])
            .expect("probe")
    );
}
