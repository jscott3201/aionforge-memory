//! Integration tests for the L0 store.
//!
//! Three properties carry the milestone-exit acceptance: an episode survives a
//! commit-then-read unchanged; hostile text bound as a parameter comes back as data
//! and never executes as GQL; and reads take an isolated snapshot that a concurrent
//! writer cannot disturb or block.

use std::sync::Arc;
use std::thread;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Origin, Redaction, Role};
use aionforge_domain::time::Timestamp;

use aionforge_store::{BoundQuery, QueryResult, Store, StoreConfig, Value};

use proptest::prelude::*;

/// Parse a fixed zoned datetime so the tests are deterministic.
fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime literal")
}

/// A store with the schema applied — the shape every typed write needs, since the
/// graph is closed and rejects a node whose kind has not been declared.
///
/// The embedding dimension is set to 4 to match [`rich_episode`]'s toy embedding: the
/// `Episode.embedding_v1` vector index is pinned at this dimension, so an embedding of a
/// different length would (correctly) be rejected at insert. This toy dimension keeps the
/// round-trip test simple; the realistic dimension-pinning and the §13.5 consistency check
/// are exercised separately in `tests/indexes.rs`, and the round-trip here covers Episode
/// (the only kind with a typed insert/read path at this layer).
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
        importance: 0.625,
        trust: 0.8,
        last_access: ts("2026-06-06T10:00:00-05:00[America/Chicago]"),
        access_count_recent: 3,
        referenced_count: 1,
        surprise: 0.125,
        is_pinned: false,
    }
}

/// A minimal episode with every nullable field absent and a fresh id.
fn episode(content: &str) -> Episode {
    Episode {
        identity: Identity {
            id: Id::generate(),
            ingested_at: ts("2026-06-06T09:30:00-05:00[America/Chicago]"),
            namespace: Namespace::Agent("alice".to_string()),
            expired_at: None,
        },
        stats: stats(),
        content: content.to_string(),
        role: Role::User,
        captured_at: ts("2026-06-06T09:29:59-05:00[America/Chicago]"),
        agent_id: Id::generate(),
        session_id: None,
        content_hash: ContentHash::of(content.as_bytes()),
        embedding: None,
        embedder_model: None,
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    }
}

/// An episode with every nullable field populated, to exercise the full translation.
fn rich_episode() -> Episode {
    let content = "the user asked about bi-temporal retrieval";
    let mut ep = episode(content);
    ep.identity.expired_at = Some(ts("2026-07-01T00:00:00-05:00[America/Chicago]"));
    ep.session_id = Some(Id::generate());
    ep.role = Role::Assistant;
    ep.consolidation_state = ConsolidationState::Consolidated;
    ep.embedding = Some(Embedding::new(vec![0.1, -0.2, 0.3, 0.4]).expect("finite embedding"));
    ep.embedder_model = Some(EmbedderModel {
        family: "bge".to_string(),
        version: "1.5".to_string(),
        dimension: 4,
    });
    ep.origin = Some(Origin {
        model_family: Some("claude".to_string()),
        model_version: Some("opus".to_string()),
        transport: Some("mcp".to_string()),
        request_id: Some("req-123".to_string()),
        redactions: vec![Redaction {
            pattern_id: "email".to_string(),
            span: (10, 25),
            kind: "pii".to_string(),
        }],
        injection_flags: vec!["ignore-previous".to_string()],
        capture_latency_ms: Some(2),
        supersedes: None,
    });
    ep
}

/// jiff `Zoned` equality compares only the underlying instant, so the derived
/// `PartialEq` on `Episode` cannot catch a dropped IANA zone or civil offset.
/// Compare the rendered form too — it carries the `[America/Chicago]` annotation
/// and nanoseconds — so a future serialization path (T06) that normalized to UTC
/// while preserving the instant would fail this round-trip instead of passing it.
fn assert_timestamps_render_identically(read: &Episode, original: &Episode) {
    assert_eq!(
        read.identity.ingested_at.to_string(),
        original.identity.ingested_at.to_string(),
        "ingested_at lost zone/precision"
    );
    assert_eq!(
        read.identity.expired_at.as_ref().map(ToString::to_string),
        original
            .identity
            .expired_at
            .as_ref()
            .map(ToString::to_string),
        "expired_at lost zone/precision"
    );
    assert_eq!(
        read.stats.last_access.to_string(),
        original.stats.last_access.to_string(),
        "last_access lost zone/precision"
    );
    assert_eq!(
        read.captured_at.to_string(),
        original.captured_at.to_string(),
        "captured_at lost zone/precision"
    );
}

#[test]
fn episode_round_trips_through_the_store() {
    let store = store();
    for original in [episode("a plain captured turn"), rich_episode()] {
        let id = store.insert_episode(&original).expect("commit episode");
        let read_back = store
            .episode_by_node_id(id)
            .expect("read episode")
            .expect("inserted episode is present");
        assert_eq!(read_back, original);
        assert_timestamps_render_identically(&read_back, &original);
    }
}

#[test]
fn batched_superseded_lookup_returns_the_newest_live_replacement() {
    let store = store();
    let old_a = episode("old alpha memory");
    let old_b = episode("old beta memory");
    let old_a_id = old_a.identity.id;
    let old_b_id = old_b.identity.id;
    store.insert_episode(&old_a).expect("insert old alpha");
    store.insert_episode(&old_b).expect("insert old beta");

    let mut first_replacement = episode("first alpha replacement");
    first_replacement.identity.ingested_at = ts("2026-06-06T09:31:00-05:00[America/Chicago]");
    first_replacement.origin = Some(origin_superseding(old_a_id));
    let first_replacement_id = first_replacement.identity.id;
    store
        .insert_episode(&first_replacement)
        .expect("insert first replacement");

    let mut newest_replacement = episode("newest alpha replacement");
    newest_replacement.identity.ingested_at = ts("2026-06-06T09:32:00-05:00[America/Chicago]");
    newest_replacement.origin = Some(origin_superseding(old_a_id));
    let newest_replacement_id = newest_replacement.identity.id;
    store
        .insert_episode(&newest_replacement)
        .expect("insert newest replacement");

    let mut beta_replacement = episode("beta replacement");
    beta_replacement.identity.ingested_at = ts("2026-06-06T09:33:00-05:00[America/Chicago]");
    beta_replacement.origin = Some(origin_superseding(old_b_id));
    let beta_replacement_id = beta_replacement.identity.id;
    store
        .insert_episode(&beta_replacement)
        .expect("insert beta replacement");

    let found = store
        .live_episode_superseded_by_many([&old_a_id, &old_b_id])
        .expect("batch lookup");
    assert_eq!(
        found.get(&old_a_id),
        Some(&newest_replacement_id),
        "newest replacement wins"
    );
    assert_eq!(found.get(&old_b_id), Some(&beta_replacement_id));
    assert_ne!(
        found.get(&old_a_id),
        Some(&first_replacement_id),
        "older replacement is not selected"
    );
    assert_eq!(
        store
            .live_episode_superseded_by(&old_a_id)
            .expect("single lookup"),
        Some(newest_replacement_id),
        "single helper delegates to the batch behavior"
    );
}

#[test]
fn known_injection_payloads_round_trip_as_data() {
    let store = store();
    // Seed real nodes so an injected mutation would change the count.
    for i in 0..3 {
        store
            .insert_episode(&episode(&format!("seed {i}")))
            .expect("seed episode");
    }
    let before = store.snapshot().node_count();

    let payloads = [
        "'; MATCH (n) DETACH DELETE n //",
        "\" OR 1=1 --",
        "$p",
        "RETURN 1 AS injected",
        "); DROP GRAPH; (",
        "line one\nline two",
        "weird \u{2603} unicode \u{6f22}\u{5b57} \u{1f525}",
    ];
    for payload in payloads {
        let query = BoundQuery::new("RETURN $p AS p")
            .bind_str("p", payload)
            .expect("bind payload");
        match store.execute(&query).expect("execute") {
            QueryResult::Rows(rows) => {
                assert_eq!(
                    rows.row_count(),
                    1,
                    "payload {payload:?} produced extra rows"
                );
                match rows.value(0, 0) {
                    Some(Value::String(v)) => assert_eq!(v.as_str(), payload),
                    other => panic!("expected the bound string back, got {other:?}"),
                }
            }
            other => panic!("expected rows, got {other:?}"),
        }
    }

    // No injected statement ran: the graph is exactly as seeded.
    assert_eq!(store.snapshot().node_count(), before);
}

fn origin_superseding(target: Id) -> Origin {
    Origin {
        model_family: Some("test".to_string()),
        model_version: None,
        transport: Some("store-test".to_string()),
        request_id: None,
        redactions: Vec::new(),
        injection_flags: Vec::new(),
        capture_latency_ms: None,
        supersedes: Some(target),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Any string, hostile or not, bound to `$p` comes back verbatim as a string
    /// value — proof that the parsed statement never depends on caller input.
    #[test]
    fn any_bound_string_round_trips_verbatim(text in "(?s).{0,256}") {
        let store = store();
        let query = BoundQuery::new("RETURN $p AS p")
            .bind_str("p", &text)
            .expect("bind text");
        match store.execute(&query).expect("execute") {
            QueryResult::Rows(rows) => {
                prop_assert_eq!(rows.row_count(), 1);
                match rows.value(0, 0) {
                    Some(Value::String(v)) => prop_assert_eq!(v.as_str(), text.as_str()),
                    other => prop_assert!(false, "expected a string, got {:?}", other),
                }
            }
            other => prop_assert!(false, "expected rows, got {:?}", other),
        }
    }
}

#[test]
fn reads_are_snapshot_isolated_from_writes() {
    let store = store();
    // The migrated store already holds the SchemaVersion singleton, so reason in
    // deltas from that baseline rather than from an empty graph.
    let baseline = store.snapshot().node_count();
    store
        .insert_episode(&episode("first"))
        .expect("first write");

    let pinned = store.snapshot();
    assert_eq!(pinned.node_count(), baseline + 1);

    store
        .insert_episode(&episode("second"))
        .expect("second write");

    // The pinned snapshot does not see the later commit (MVCC isolation)...
    assert_eq!(pinned.node_count(), baseline + 1);
    // ...but a fresh snapshot does.
    assert_eq!(store.snapshot().node_count(), baseline + 2);
}

#[test]
fn reads_make_progress_during_a_concurrent_writer() {
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, Ordering};

    const WRITES: usize = 200;
    const READERS: usize = 4;

    let store = Arc::new(store());
    // The migrated store starts with the SchemaVersion singleton; the writer adds
    // WRITES episodes on top, so the visible count climbs from baseline to baseline +
    // WRITES.
    let baseline = store.snapshot().node_count();
    let target = baseline + WRITES;
    let done = Arc::new(AtomicBool::new(false));
    // Readers and the writer (this thread) all release from the barrier together,
    // so the readers are guaranteed to be sampling while the writer commits.
    let start = Arc::new(Barrier::new(READERS + 1));

    let readers: Vec<_> = (0..READERS)
        .map(|_| {
            let store = Arc::clone(&store);
            let done = Arc::clone(&done);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                let mut saw_intermediate = false;
                // Spin on lock-free snapshots until the writer signals completion
                // and the final state is visible. A reader that blocked on the
                // writer's commit could never keep sampling like this; it would be
                // parked, not observing the count climb.
                loop {
                    let count = store.snapshot().node_count();
                    assert!(
                        count <= target,
                        "snapshot observed {count} nodes, over the {target} cap"
                    );
                    if count > baseline && count < target {
                        saw_intermediate = true;
                    }
                    if done.load(Ordering::Acquire) && count == target {
                        break;
                    }
                }
                saw_intermediate
            })
        })
        .collect();

    start.wait();
    for i in 0..WRITES {
        store
            .insert_episode(&episode(&format!("event {i}")))
            .expect("concurrent write");
    }
    done.store(true, Ordering::Release);

    // Join every reader (no short-circuit, so a panic or deadlock in any of them
    // surfaces) and record whether any saw the graph mid-fill.
    let mut saw_intermediate = false;
    for reader in readers {
        if reader.join().expect("reader did not panic or deadlock") {
            saw_intermediate = true;
        }
    }

    // At least one reader observed the graph mid-fill — proof the readers ran
    // concurrently with the committing writer and were never blocked by it.
    assert!(
        saw_intermediate,
        "no reader saw an intermediate node count; readers did not overlap the writer"
    );
    // Every write landed and is visible on a fresh snapshot.
    assert_eq!(store.snapshot().node_count(), target);
}
