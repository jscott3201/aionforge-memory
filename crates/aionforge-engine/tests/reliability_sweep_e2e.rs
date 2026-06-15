//! The reliability-sweep round-trip drift guard (06 §5, M4.T05 PR-E2): drive the REAL
//! pipeline — rule extraction, high-trust contradiction detection, the scheduler's
//! co-committed quarantine audit — then sweep. A change to the emitter's reason string or
//! payload shape fails here, where the unit table of hand-built rows in
//! `reliability_sweep.rs` could silently drift.

mod common;

use std::sync::Arc;

use aionforge_consolidate::{
    ConsolidationConfig, Consolidator, DetectionConfig, FactExtractionPass, InductionConfig,
    ObjectRule, PassConfig, PredicateRule, ResolutionConfig, Rule, RuleExtractor, RuleSummarizer,
    SummarizationConfig,
};
use aionforge_domain::blocks::{Identity, Stats};
use aionforge_domain::ids::{ContentHash, Id};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};
use aionforge_domain::value::ObjectValue;
use common::*;

#[tokio::test]
async fn a_real_contradiction_quarantine_is_swept_end_to_end() {
    let store = migrated_store();
    let namespace = Namespace::Agent("ops".to_string());

    // E1 (trust 0.9, agent ada) says the server is up; E2 (trust 0.5, agent bo) says it is
    // down. The contradiction quarantines the LOWER-trust side — bo's "down" fact — so bo is
    // the producer the sweep must decay.
    let ada = enroll(&store);
    let bo = enroll(&store);
    for (minute, content, trust, agent) in [
        (1u32, "Server status up.", 0.9, ada),
        (5, "Server status down.", 0.5, bo),
    ] {
        let at = ts(minute);
        let episode = Episode {
            identity: Identity {
                id: Id::generate(),
                ingested_at: at.clone(),
                namespace: namespace.clone(),
                expired_at: None,
            },
            stats: Stats {
                trust,
                ..stats(trust)
            },
            content: content.to_string(),
            role: Role::User,
            captured_at: at,
            agent_id: agent,
            session_id: None,
            content_hash: ContentHash::of(content.as_bytes()),
            embedding: None,
            embedder_model: None,
            consolidation_state: ConsolidationState::Raw,
            origin: None,
        };
        store.insert_episode(&episode).expect("insert episode");
    }

    // The detection fixture's status rule: "X status Y." extracts (X, status, Y), with
    // up/down declared mutually contradictory.
    let extractor = RuleExtractor::new(
        "rule-status",
        vec![Rule {
            marker: "status".to_string(),
            predicate: "status".to_string(),
            subject_type: "Service".to_string(),
            object: ObjectRule::Text,
            confidence: 0.9,
        }],
    );
    let mut detection = DetectionConfig::with_default_rules();
    detection.predicates.insert(
        "status".to_string(),
        PredicateRule {
            functional: false,
            contradicts: vec![(
                ObjectValue::Text("up".to_string()),
                ObjectValue::Text("down".to_string()),
            )],
        },
    );
    let mut consolidator = Consolidator::new(Arc::clone(&store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(extractor),
        Arc::new(AxisEmbedder::new()),
        Arc::new(RuleSummarizer::with_default_rules()),
        PassConfig {
            resolution: ResolutionConfig::default(),
            detection,
            summarization: SummarizationConfig::default(),
            induction: InductionConfig::default(),
            ..PassConfig::default()
        },
    )));
    loop {
        let report = consolidator.tick_once().await.expect("tick");
        if report.pending_after == 0 {
            break;
        }
    }

    let memory = memory(&store);
    let report = memory
        .sweep_reliability_decays(None, 50, &ts(30))
        .expect("sweep");
    assert_eq!(
        report.quarantines_scanned, 1,
        "the pipeline-emitted quarantine row classifies as a D1 trigger: {report:?}"
    );
    assert_eq!(report.decays_recorded, 1, "the victim's producer pays");
    assert_eq!(report.victims_unresolved, 0);
    assert!(
        (agent_score_in(&store, &bo, "status").expect("bo scored") - 1.0 / 3.0).abs() < EPS,
        "the lower-trust producer decays in the fact's category"
    );
    assert!(
        agent_score_in(&store, &ada, "status").is_none(),
        "the survivor's producer is untouched"
    );
}
