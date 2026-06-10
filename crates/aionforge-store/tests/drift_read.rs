//! Store-level tests for the M5.T05 drift plumbing: the behavior-window read (the
//! drift sweep's raw material) and the `cooled_until` column round-trip (the cooling
//! stamp the rank-time modulation reads).

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::nodes::semantic::{Fact, FactStatus};
use aionforge_domain::time::Timestamp;
use aionforge_domain::value::ObjectValue;
use aionforge_store::{Store, StoreConfig};

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime")
}

fn at(hour: u32) -> Timestamp {
    ts(&format!(
        "2026-06-10T{hour:02}:00:00-05:00[America/Chicago]"
    ))
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

fn agent_ns() -> Namespace {
    Namespace::Agent("behavior-owner".to_string())
}

fn seed_episode(
    store: &Store,
    hour: u32,
    seed: u8,
    namespace: Namespace,
    embedding: Option<[f32; 4]>,
    expired: bool,
) -> Id {
    let id = Id::from_content_hash(&[seed]);
    let episode = Episode {
        identity: Identity {
            id,
            ingested_at: at(hour),
            namespace,
            expired_at: expired.then(|| at(23)),
        },
        stats: Stats {
            importance: 0.5,
            trust: 0.8,
            last_access: at(hour),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.0,
            is_pinned: false,
        },
        content: format!("episode {seed}"),
        role: Role::User,
        captured_at: at(hour),
        agent_id: Id::from_content_hash(b"writer"),
        session_id: None,
        content_hash: ContentHash::of(&[seed]),
        embedding: embedding.map(|v| Embedding::new(v.to_vec()).expect("finite embedding")),
        embedder_model: embedding.map(|_| EmbedderModel {
            family: "fake".to_string(),
            version: "1".to_string(),
            dimension: 4,
        }),
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("seed episode");
    id
}

#[test]
fn the_behavior_read_scopes_filters_and_orders_canonically() {
    let store = store();
    // In-window, embedded, live: hours 10, 12, 14 (seeds 1..3).
    seed_episode(&store, 12, 1, agent_ns(), Some([0.0, 1.0, 0.0, 0.0]), false);
    seed_episode(&store, 10, 2, agent_ns(), Some([1.0, 0.0, 0.0, 0.0]), false);
    seed_episode(&store, 14, 3, agent_ns(), Some([0.0, 0.0, 1.0, 0.0]), false);
    // Excluded: outside the window (before/at-the-until-bound), other namespace,
    // soft-forgotten, unembedded.
    seed_episode(&store, 2, 4, agent_ns(), Some([1.0, 1.0, 0.0, 0.0]), false);
    seed_episode(&store, 18, 5, agent_ns(), Some([1.0, 1.0, 0.0, 0.0]), false);
    seed_episode(
        &store,
        12,
        6,
        Namespace::Agent("someone-else".to_string()),
        Some([1.0, 1.0, 0.0, 0.0]),
        false,
    );
    seed_episode(&store, 12, 7, agent_ns(), Some([1.0, 1.0, 0.0, 0.0]), true);
    seed_episode(&store, 12, 8, agent_ns(), None, false);

    let sample = store
        .recent_embedded_episodes(&agent_ns(), &at(9), &at(18), 10)
        .expect("read");
    assert_eq!(sample.len(), 3, "exactly the in-window embedded live rows");
    // Ascending canonical order: hour 10, 12, 14.
    assert_eq!(sample[0].embedding.as_slice(), &[1.0, 0.0, 0.0, 0.0]);
    assert_eq!(sample[1].embedding.as_slice(), &[0.0, 1.0, 0.0, 0.0]);
    assert_eq!(sample[2].embedding.as_slice(), &[0.0, 0.0, 1.0, 0.0]);
    assert!(
        sample.iter().all(|v| v
            .embedder_model
            .as_ref()
            .is_some_and(|m| m.family == "fake")),
        "the model identity rides along for the cross-space guard"
    );
}

#[test]
fn the_cap_keeps_the_most_recent_and_returns_ascending() {
    let store = store();
    for (hour, seed) in [(8u32, 10u8), (10, 11), (12, 12), (14, 13)] {
        let component = f32::from(seed);
        seed_episode(
            &store,
            hour,
            seed,
            agent_ns(),
            Some([component, 1.0, 0.0, 0.0]),
            false,
        );
    }
    let sample = store
        .recent_embedded_episodes(&agent_ns(), &at(0), &at(23), 2)
        .expect("read");
    assert_eq!(sample.len(), 2, "the cap bounds the sample");
    // The two MOST RECENT (hours 12 and 14), in ascending order.
    assert_eq!(sample[0].embedding.as_slice()[0], 12.0);
    assert_eq!(sample[1].embedding.as_slice()[0], 13.0);

    let empty = store
        .recent_embedded_episodes(&agent_ns(), &at(0), &at(23), 0)
        .expect("read");
    assert!(empty.is_empty(), "a zero cap reads nothing");
}

#[test]
fn the_cooling_stamp_round_trips_and_defaults_to_never_cooled() {
    let store = store();
    let base = Fact {
        identity: Identity {
            id: Id::from_content_hash(b"uncooled"),
            ingested_at: at(10),
            namespace: agent_ns(),
            expired_at: None,
        },
        stats: Stats {
            importance: 0.5,
            trust: 0.8,
            last_access: at(10),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.0,
            is_pinned: false,
        },
        subject_id: Id::from_content_hash(b"subject"),
        predicate: "prefers".to_string(),
        object: ObjectValue::Text("plain text".to_string()),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: "the subject prefers plain text".to_string(),
        embedding: None,
        embedder_model: None,
        extraction: None,
        cooled_until: None,
    };
    let node = store.insert_fact(&base).expect("insert");
    let read = store.fact_by_node_id(node).expect("read").expect("present");
    assert_eq!(
        read.cooled_until, None,
        "absent stamp reads as never cooled"
    );

    let mut cooled = base.clone();
    cooled.identity.id = Id::from_content_hash(b"cooled");
    cooled.cooled_until = Some(at(20));
    let node = store.insert_fact(&cooled).expect("insert");
    let read = store.fact_by_node_id(node).expect("read").expect("present");
    assert_eq!(
        read.cooled_until,
        Some(at(20)),
        "the stamp round-trips through the column"
    );
}
