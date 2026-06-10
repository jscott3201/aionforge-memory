//! Acceptance tests for the off-cursor cooling sweep (05 §1, M5.T05): the
//! namespace-scoped attested anchors, every non-cooling condition, the stamp-once
//! idempotency with its co-committed audit, and the watermark walk.

use std::sync::Arc;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::core::{BlockKind, CoreBlock};
use aionforge_domain::nodes::forensic::{AuditEvent, AuditKind};
use aionforge_domain::nodes::semantic::{Fact, FactStatus};
use aionforge_domain::time::{Timestamp, instant_after};
use aionforge_domain::value::ObjectValue;
use aionforge_forget::{DriftBaseline, DriftDetector, DriftPolicy};
use aionforge_store::{NodeId, Store, StoreConfig};

fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid zoned datetime")
}

fn at(hour: u32) -> Timestamp {
    ts(&format!(
        "2026-06-10T{hour:02}:00:00-05:00[America/Chicago]"
    ))
}

fn store() -> Arc<Store> {
    let store = Store::open_with_config(StoreConfig {
        embedding_dimension: 4,
    })
    .expect("open store");
    store
        .migrate(&ts("2026-01-01T00:00:00-06:00[America/Chicago]"))
        .expect("migrate store");
    Arc::new(store)
}

fn agent_ns() -> Namespace {
    Namespace::Agent("behavior-owner".to_string())
}

fn model() -> EmbedderModel {
    EmbedderModel {
        family: "fake".to_string(),
        version: "1".to_string(),
        dimension: 4,
    }
}

fn foreign_model() -> EmbedderModel {
    EmbedderModel {
        family: "other".to_string(),
        version: "2".to_string(),
        dimension: 4,
    }
}

fn policy() -> DriftPolicy {
    DriftPolicy {
        enabled: true,
        cooling_window_secs: 3_600,
        ..DriftPolicy::default()
    }
}

fn embedding(vector: [f32; 4]) -> Embedding {
    Embedding::new(vector.to_vec()).expect("finite embedding")
}

fn stats(trust: f64) -> Stats {
    Stats {
        importance: 0.9,
        trust,
        last_access: at(1),
        access_count_recent: 0,
        referenced_count: 0,
        surprise: 0.0,
        is_pinned: false,
    }
}

fn insert_block(
    store: &Store,
    seed: &[u8],
    trust: f64,
    anchor: Option<([f32; 4], EmbedderModel)>,
) -> Id {
    let id = Id::from_content_hash(seed);
    let content = format!("commitment {id}");
    let baseline = anchor.map(|(vector, anchor_model)| {
        DriftBaseline {
            v: DriftBaseline::VERSION,
            embedder_model: anchor_model,
            content_hash: ContentHash::of(content.as_bytes()),
            block_embedding: embedding(vector),
            behavior_centroid: None,
            baselined_at: at(2),
            window_secs: 604_800,
            sample_size: 0,
        }
        .to_value()
    });
    let block = CoreBlock {
        identity: Identity {
            id,
            ingested_at: at(1),
            namespace: agent_ns(),
            expired_at: None,
        },
        stats: stats(trust),
        content,
        block_kind: BlockKind::Commitment,
        sensitivity: None,
        drift_baseline: baseline,
        embedding: None,
        embedder_model: None,
    };
    let audit = AuditEvent {
        identity: Identity {
            id: Id::generate(),
            ingested_at: at(1),
            namespace: agent_ns(),
            expired_at: None,
        },
        kind: AuditKind::CoreEdit,
        subject_id: id,
        actor_id: Id::from_content_hash(b"test-host"),
        payload: serde_json::json!({"outcome": "created"}),
        signature: String::new(),
        occurred_at: at(1),
    };
    store
        .create_core_block(&block, &audit)
        .expect("create block");
    id
}

#[allow(clippy::too_many_arguments)]
fn insert_fact(
    store: &Store,
    hour: u32,
    seed: u8,
    vector: Option<[f32; 4]>,
    fact_model: Option<EmbedderModel>,
    cooled_until: Option<Timestamp>,
    expired: bool,
) -> (NodeId, Id) {
    let id = Id::from_content_hash(&[seed]);
    let fact = Fact {
        identity: Identity {
            id,
            ingested_at: at(hour),
            namespace: agent_ns(),
            expired_at: expired.then(|| at(23)),
        },
        stats: stats(0.8),
        subject_id: Id::from_content_hash(b"subject"),
        predicate: "claims".to_string(),
        object: ObjectValue::Text(format!("claim {seed}")),
        confidence: 0.9,
        status: FactStatus::Active,
        statement: format!("claim {seed}"),
        embedding: vector.map(embedding),
        embedder_model: fact_model,
        extraction: None,
        cooled_until,
    };
    let node = store.insert_fact(&fact).expect("insert fact");
    (node, id)
}

fn cooled_audits(store: &Store) -> Vec<AuditEvent> {
    store
        .audit_by_kind(AuditKind::Cooled, None, 50)
        .expect("audit read")
        .events
}

const E0: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
const E1: [f32; 4] = [0.0, 1.0, 0.0, 0.0];
const E2: [f32; 4] = [0.0, 0.0, 1.0, 0.0];

#[test]
fn the_sweep_stamps_exactly_the_proximate_and_only_once() {
    let store = store();
    let detector = DriftDetector::new(Arc::clone(&store), policy());
    // Anchors: a high-trust block anchored on E0; a low-trust block anchored on E2
    // (below the trust bar — proximity to it cools nothing); a high-trust block
    // with no attested baseline (no anchor at all).
    let anchor_block = insert_block(&store, b"anchor", 0.9, Some((E0, model())));
    insert_block(&store, b"low-trust", 0.3, Some((E2, model())));
    insert_block(&store, b"unseeded", 0.9, None);

    // Facts, in ingestion order:
    let (proximate, proximate_id) =
        insert_fact(&store, 10, 1, Some(E0), Some(model()), None, false);
    let (far, _) = insert_fact(&store, 11, 2, Some(E1), Some(model()), None, false);
    let (foreign, _) = insert_fact(&store, 12, 3, Some(E0), Some(foreign_model()), None, false);
    let (unembedded, _) = insert_fact(&store, 13, 4, None, None, None, false);
    let (near_low_trust, _) = insert_fact(&store, 14, 5, Some(E2), Some(model()), None, false);
    let pre_stamp = at(9);
    let (already, _) = insert_fact(
        &store,
        15,
        6,
        Some(E0),
        Some(model()),
        Some(pre_stamp.clone()),
        false,
    );
    insert_fact(&store, 16, 7, Some(E0), Some(model()), None, true); // soft-forgotten

    let now = at(18);
    let report = detector.sweep_cooling(None, 50, &now).expect("sweep");
    assert_eq!(
        report.facts_scanned, 6,
        "the forgotten fact is never visited"
    );
    assert_eq!(
        report.facts_cooled, 1,
        "only the same-space, high-trust-proximate fact"
    );
    assert!(report.next.is_some());

    // The stamp: now + the policy window, on the proximate fact only.
    let read = |node| {
        store
            .fact_by_node_id(node)
            .expect("read")
            .expect("present")
            .cooled_until
    };
    assert_eq!(
        read(proximate),
        Some(instant_after(&now, 3_600)),
        "stamped one cooling window out"
    );
    assert_eq!(read(far), None);
    assert_eq!(read(foreign), None, "cross-space is never proximate");
    assert_eq!(read(unembedded), None);
    assert_eq!(
        read(near_low_trust),
        None,
        "a low-trust block cools nothing"
    );
    assert_eq!(
        read(already),
        Some(pre_stamp),
        "never re-stamped or extended"
    );

    // One Cooled audit row, in the fact's namespace, naming the anchoring block.
    let audits = cooled_audits(&store);
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].subject_id, proximate_id);
    assert_eq!(audits[0].identity.namespace, agent_ns());
    assert_eq!(
        audits[0].payload["proximate_block"],
        serde_json::json!(anchor_block.to_string())
    );

    // Re-sweeping the same ground is a true no-op: nothing stamped, no second row.
    let again = detector.sweep_cooling(None, 50, &at(19)).expect("sweep");
    assert_eq!(again.facts_cooled, 0);
    assert_eq!(cooled_audits(&store).len(), 1);
}

#[test]
fn the_watermark_pages_through_in_ingestion_order() {
    let store = store();
    let detector = DriftDetector::new(Arc::clone(&store), policy());
    insert_block(&store, b"anchor", 0.9, Some((E0, model())));
    for (hour, seed) in [(10u32, 1u8), (11, 2), (12, 3)] {
        insert_fact(&store, hour, seed, Some(E0), Some(model()), None, false);
    }

    let now = at(18);
    let first = detector.sweep_cooling(None, 2, &now).expect("sweep");
    assert_eq!(first.facts_scanned, 2);
    assert_eq!(first.facts_cooled, 2);
    let cursor = first.next.expect("watermark");

    let second = detector
        .sweep_cooling(Some(&cursor), 2, &now)
        .expect("sweep");
    assert_eq!(
        second.facts_scanned, 1,
        "resumes strictly after the watermark"
    );
    assert_eq!(second.facts_cooled, 1);

    let cursor = second.next.expect("watermark");
    let empty = detector
        .sweep_cooling(Some(&cursor), 2, &now)
        .expect("sweep");
    assert_eq!(empty.facts_scanned, 0);
    assert_eq!(empty.next, None, "an empty page ends the walk");
    assert_eq!(cooled_audits(&store).len(), 3);
}
