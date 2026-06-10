//! Acceptance tests for the drift detector (05 §1, M5.T05): the store-backed
//! behavior centroid (window, model-space filter, sample floor) and the per-block
//! assessment matrix — every skip named, scores only where the arithmetic can vouch.

use std::sync::Arc;

use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::core::{BlockKind, CoreBlock};
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::time::Timestamp;
use aionforge_forget::{
    BaselineNeed, BlockAssessment, CentroidOutcome, DriftBaseline, DriftDetector, DriftPolicy,
};
use aionforge_store::{Store, StoreConfig};

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
        min_sample_size: 2,
        behavior_sample_size: 8,
        ..DriftPolicy::default()
    }
}

fn seed_episode(store: &Store, hour: u32, seed: u8, vector: [f32; 4], model: EmbedderModel) {
    let id = Id::from_content_hash(&[seed]);
    let episode = Episode {
        identity: Identity {
            id,
            ingested_at: at(hour),
            namespace: agent_ns(),
            expired_at: None,
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
        embedding: Some(Embedding::new(vector.to_vec()).expect("finite embedding")),
        embedder_model: Some(model),
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    };
    store.insert_episode(&episode).expect("seed episode");
}

fn embedding(vector: [f32; 4]) -> Embedding {
    Embedding::new(vector.to_vec()).expect("finite embedding")
}

fn baseline_for(content: &str, behavior: Option<[f32; 4]>) -> DriftBaseline {
    DriftBaseline {
        v: DriftBaseline::VERSION,
        embedder_model: model(),
        content_hash: ContentHash::of(content.as_bytes()),
        block_embedding: embedding([1.0, 0.0, 0.0, 0.0]),
        behavior_centroid: behavior.map(embedding),
        baselined_at: at(6),
        window_secs: 604_800,
        sample_size: if behavior.is_some() { 4 } else { 0 },
    }
}

fn block_with(content: &str, baseline: Option<serde_json::Value>) -> CoreBlock {
    CoreBlock {
        identity: Identity {
            id: Id::from_content_hash(b"commitment"),
            ingested_at: at(5),
            namespace: agent_ns(),
            expired_at: None,
        },
        stats: Stats {
            importance: 1.0,
            trust: 0.9,
            last_access: at(5),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.0,
            is_pinned: false,
        },
        content: content.to_string(),
        block_kind: BlockKind::Commitment,
        sensitivity: None,
        drift_baseline: baseline,
        embedding: None,
        embedder_model: None,
    }
}

#[test]
fn the_centroid_reads_only_the_live_space_inside_the_window() {
    let store = store();
    // Comparable: two e1-direction episodes inside the window.
    seed_episode(&store, 10, 1, [0.0, 1.0, 0.0, 0.0], model());
    seed_episode(&store, 12, 2, [0.0, 1.0, 0.0, 0.0], model());
    // Dropped: foreign embedding space, and outside the window (8 days back).
    seed_episode(&store, 11, 3, [1.0, 0.0, 0.0, 0.0], foreign_model());
    {
        let old = ts("2026-06-01T10:00:00-05:00[America/Chicago]");
        let episode = Episode {
            identity: Identity {
                id: Id::from_content_hash(&[4]),
                ingested_at: old.clone(),
                namespace: agent_ns(),
                expired_at: None,
            },
            stats: Stats {
                importance: 0.5,
                trust: 0.8,
                last_access: old.clone(),
                access_count_recent: 0,
                referenced_count: 0,
                surprise: 0.0,
                is_pinned: false,
            },
            content: "stale".to_string(),
            role: Role::User,
            captured_at: old,
            agent_id: Id::from_content_hash(b"writer"),
            session_id: None,
            content_hash: ContentHash::of(&[4]),
            embedding: Some(embedding([1.0, 0.0, 0.0, 0.0])),
            embedder_model: Some(model()),
            consolidation_state: ConsolidationState::Raw,
            origin: None,
        };
        store.insert_episode(&episode).expect("seed episode");
    }

    let detector = DriftDetector::new(Arc::clone(&store), policy());
    let outcome = detector
        .behavior_centroid_now(&agent_ns(), &model(), &at(18))
        .expect("centroid read");
    match outcome {
        CentroidOutcome::Centroid {
            centroid,
            sample_size,
        } => {
            assert_eq!(sample_size, 2, "only the comparable in-window episodes");
            assert_eq!(
                centroid.as_slice(),
                &[0.0, 1.0, 0.0, 0.0],
                "the normalized mean of an aligned sample is the shared direction"
            );
        }
        other => panic!("expected a centroid, got {other:?}"),
    }
}

#[test]
fn a_thin_or_foreign_sample_is_a_named_skip_never_a_guess() {
    let store = store();
    // One comparable episode (floor is two) and one in a foreign space.
    seed_episode(&store, 10, 1, [0.0, 1.0, 0.0, 0.0], model());
    seed_episode(&store, 12, 2, [0.0, 1.0, 0.0, 0.0], foreign_model());

    let detector = DriftDetector::new(Arc::clone(&store), policy());
    let outcome = detector
        .behavior_centroid_now(&agent_ns(), &model(), &at(18))
        .expect("centroid read");
    assert_eq!(
        outcome,
        CentroidOutcome::InsufficientSample { have: 1, need: 2 },
        "the foreign-space episode must not count toward the floor"
    );
}

#[test]
fn the_assessment_matrix_names_every_skip() {
    let store = store();
    let detector = DriftDetector::new(Arc::clone(&store), policy());
    let live = model();
    let current = CentroidOutcome::Centroid {
        centroid: embedding([0.0, 1.0, 0.0, 0.0]),
        sample_size: 4,
    };
    let content = "never deploy on friday";

    // No baseline ever attested.
    assert_eq!(
        detector.assess_block(&block_with(content, None), &live, &current),
        BlockAssessment::NeedsBaseline(BaselineNeed::Missing)
    );

    // Stored JSON that is not the schema.
    let garbage = block_with(content, Some(serde_json::json!({"summary": "old shape"})));
    assert!(matches!(
        detector.assess_block(&garbage, &live, &current),
        BlockAssessment::InvalidBaseline { .. }
    ));

    // Baseline attested under a different embedder.
    let stale = DriftBaseline {
        embedder_model: foreign_model(),
        ..baseline_for(content, Some([1.0, 0.0, 0.0, 0.0]))
    };
    assert_eq!(
        detector.assess_block(
            &block_with(content, Some(stale.to_value())),
            &live,
            &current
        ),
        BlockAssessment::StaleModel
    );

    // Content edited since attestation: the anchor no longer describes the block.
    let anchored_elsewhere = baseline_for("an earlier wording", Some([1.0, 0.0, 0.0, 0.0]));
    assert_eq!(
        detector.assess_block(
            &block_with(content, Some(anchored_elsewhere.to_value())),
            &live,
            &current
        ),
        BlockAssessment::NeedsBaseline(BaselineNeed::ContentChanged)
    );

    // Genesis baseline: attested before any behavior was observed.
    let genesis = baseline_for(content, None);
    assert_eq!(
        detector.assess_block(
            &block_with(content, Some(genesis.to_value())),
            &live,
            &current
        ),
        BlockAssessment::AwaitingFirstBehavior
    );

    // Sound baseline, but the namespace sample sits below the floor.
    let sound = baseline_for(content, Some([1.0, 0.0, 0.0, 0.0]));
    assert_eq!(
        detector.assess_block(
            &block_with(content, Some(sound.to_value())),
            &live,
            &CentroidOutcome::InsufficientSample { have: 1, need: 2 }
        ),
        BlockAssessment::InsufficientSample { have: 1, need: 2 }
    );
}

#[test]
fn scores_movement_away_from_the_anchor_and_only_that() {
    let store = store();
    let detector = DriftDetector::new(Arc::clone(&store), policy());
    let live = model();
    let content = "never deploy on friday";
    // At baseline time behavior sat ON the anchor (cosine 1); now it is orthogonal
    // (cosine 0): the full unit of drift.
    let sound = baseline_for(content, Some([1.0, 0.0, 0.0, 0.0]));
    let block = block_with(content, Some(sound.to_value()));
    let away = CentroidOutcome::Centroid {
        centroid: embedding([0.0, 1.0, 0.0, 0.0]),
        sample_size: 4,
    };
    match detector.assess_block(&block, &live, &away) {
        BlockAssessment::Scored {
            score,
            crossed,
            baselined_at,
        } => {
            assert!((score - 1.0).abs() < 1e-9, "full drift scores 1.0: {score}");
            assert!(crossed, "1.0 crosses the 0.15 default threshold");
            assert_eq!(
                baselined_at,
                at(6),
                "the score names the baseline epoch it measured against"
            );
        }
        other => panic!("expected a score, got {other:?}"),
    }

    // Behavior still on the anchor: no drift, never crosses.
    let steady = CentroidOutcome::Centroid {
        centroid: embedding([1.0, 0.0, 0.0, 0.0]),
        sample_size: 4,
    };
    assert_eq!(
        detector.assess_block(&block, &live, &steady),
        BlockAssessment::Scored {
            score: 0.0,
            crossed: false,
            baselined_at: at(6),
        }
    );
}
