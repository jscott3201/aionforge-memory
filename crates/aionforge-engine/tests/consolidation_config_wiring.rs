//! Wiring test for the `[consolidation]` deployment-config path (Steward #1): a `Memory`
//! built with non-default consolidation/pass config retains it and exposes it via the
//! `consolidation_config()` / `pass_config()` accessors — the exact values
//! `consolidate_tool` reads instead of `::default()`. We also drive `consolidate_once` with
//! the accessor-provided values to prove the consolidate path accepts them end to end.
//!
//! Hermetic: a fake embedder stands in for the network client.

use std::future::Future;
use std::time::Duration;

use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::time::Timestamp;
use aionforge_engine::{
    ConsolidationConfig, InductionConfig, Memory, MemoryConfig, PassConfig, ResolutionConfig,
    RuleExtractor, RuleInducer, RuleSummarizer, SummarizationConfig,
};

#[derive(Clone)]
struct FakeEmbedder {
    model: EmbedderModel,
}

impl FakeEmbedder {
    fn new() -> Self {
        Self {
            model: EmbedderModel {
                family: "fake".to_string(),
                version: "1".to_string(),
                dimension: 4,
            },
        }
    }
}

#[derive(Debug)]
struct NeverFails;

impl std::fmt::Display for NeverFails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unreachable")
    }
}

impl std::error::Error for NeverFails {}

impl Embedder for FakeEmbedder {
    type Error = NeverFails;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let out = inputs
            .iter()
            .map(|_| Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("valid"))
            .collect();
        async move { Ok(out) }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

fn now() -> Timestamp {
    "2026-06-06T09:30:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

/// A non-default consolidation/pass config: distinct scheduler bounds, induction turned ON,
/// and lowered summarization floors — the shape a `[consolidation]` block would produce.
fn non_default_config() -> (ConsolidationConfig, PassConfig) {
    let scheduler = ConsolidationConfig {
        tick_interval: Duration::from_secs(11),
        batch_size: 7,
        apply_timeout: Duration::from_secs(90),
        max_retries: 9,
        lag_ceiling: Duration::from_secs(13),
    };
    let pass = PassConfig {
        resolution: ResolutionConfig {
            candidate_k: 16,
            merge_threshold: 0.05,
        },
        summarization: SummarizationConfig {
            enabled: true,
            min_facts: 2,
            min_entities: 1,
            entity_retention_threshold: 0.8,
            confidence_floor: 0.5,
        },
        induction: InductionConfig {
            enabled: true,
            name_prefix: "skill/".to_string(),
            ..InductionConfig::default()
        },
        ..PassConfig::default()
    };
    (scheduler, pass)
}

#[test]
fn the_accessors_return_the_configured_consolidation_values() {
    let (scheduler, pass) = non_default_config();
    let config = MemoryConfig {
        consolidation: scheduler.clone(),
        pass: pass.clone(),
        ..MemoryConfig::default()
    };
    let memory = Memory::open_in_memory(FakeEmbedder::new(), &now(), config).expect("open");

    // The exact values `consolidate_tool` passes to `consolidate_once`.
    assert_eq!(
        memory.consolidation_config(),
        scheduler,
        "the retained scheduler config is readable knob for knob"
    );
    assert_eq!(
        memory.pass_config(),
        pass,
        "the retained pass tuning is readable knob for knob"
    );
    // Spot-check the load-bearing knobs the deployment cares about.
    assert_eq!(memory.consolidation_config().batch_size, 7);
    assert_eq!(
        memory.consolidation_config().tick_interval,
        Duration::from_secs(11)
    );
    assert!(
        memory.pass_config().induction.enabled,
        "induction reached the engine through the accessor"
    );
    assert_eq!(memory.pass_config().induction.name_prefix, "skill/");
    assert_eq!(memory.pass_config().summarization.min_facts, 2);
}

#[test]
fn the_default_config_accessors_equal_the_engine_defaults() {
    // No behavior change when [consolidation] is absent: the default MemoryConfig exposes the
    // engine `::default()` values through the same accessors.
    let memory =
        Memory::open_in_memory(FakeEmbedder::new(), &now(), MemoryConfig::default()).expect("open");
    assert_eq!(
        memory.consolidation_config(),
        ConsolidationConfig::default()
    );
    assert_eq!(memory.pass_config(), PassConfig::default());
}

#[tokio::test]
async fn consolidate_once_accepts_the_accessor_provided_values() {
    // The consolidate path (the same call `consolidate_tool` makes) runs with the configured
    // values fed from the accessors, on an empty store: a clean zero-work tick, no panic.
    let (scheduler, pass) = non_default_config();
    let config = MemoryConfig {
        consolidation: scheduler,
        pass,
        ..MemoryConfig::default()
    };
    let memory = Memory::open_in_memory(FakeEmbedder::new(), &now(), config).expect("open");

    let report = memory
        .consolidate_once(
            RuleExtractor::with_default_rules(),
            RuleSummarizer::with_default_rules(),
            RuleInducer::with_default_rules(),
            memory.consolidation_config(),
            memory.pass_config(),
        )
        .await
        .expect("a tick with the configured values runs");
    assert_eq!(
        report.consolidated, 0,
        "nothing to consolidate on an empty store"
    );
    assert_eq!(report.pending_after, 0);
}
