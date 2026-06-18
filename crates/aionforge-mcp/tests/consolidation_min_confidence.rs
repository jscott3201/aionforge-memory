//! The deployment `[consolidation.extraction] min_confidence` floor is live through the
//! PRODUCTION consolidation path, not just inside the in-crate rule extractor.
//!
//! `consolidate_tool` builds the rule extractor itself from `memory.pass_config().extraction`,
//! so a deployment's confidence floor must reach that extractor. The earlier wiring built the
//! extractor with the parameterless `with_default_rules()` (a hardcoded default floor), which
//! made the knob inert; this binary exercises the tool end to end so a regression to that
//! inert wiring fails fast. Kept apart from `lifecycle.rs` to hold both test files under the
//! per-file LOC cap.

use std::sync::Arc;

use aionforge_domain::ids::Id;
use aionforge_engine::{ExtractionConfig, Memory, MemoryConfig, PassConfig};
use aionforge_mcp::{
    AuthEnabled, CaptureToolParams, ConsolidationRunToolParams, capture_tool, consolidate_tool,
};
use aionforge_store::{BoundQuery, QueryResult, Value};

mod common;

use common::{FakeEmbedder, now};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn memory_with_config(config: MemoryConfig) -> Arc<Memory<FakeEmbedder>> {
    Arc::new(Memory::open_in_memory(FakeEmbedder::new(), &now(), config).expect("open memory"))
}

/// A `Memory` at the default deployment config — the default `min_confidence` floor is `0.8`.
fn default_memory() -> Arc<Memory<FakeEmbedder>> {
    memory_with_config(MemoryConfig::default())
}

/// A `Memory` whose deployment `[consolidation.extraction] min_confidence` floor is set,
/// exactly as the CLI host folds `MemoryConfig` into the facade. The floor is what
/// `consolidate_tool` must thread into the rule extractor it builds.
fn memory_with_min_confidence(min_confidence: f64) -> Arc<Memory<FakeEmbedder>> {
    memory_with_config(MemoryConfig {
        pass: PassConfig {
            extraction: ExtractionConfig {
                min_confidence,
                ..ExtractionConfig::default()
            },
            ..PassConfig::default()
        },
        ..MemoryConfig::default()
    })
}

fn capture_params(content: &str, agent_id: &str) -> CaptureToolParams {
    CaptureToolParams {
        content: content.to_string(),
        agent_id: Some(agent_id.to_string()),
        principal: None,
        teams: Vec::new(),
        target_namespace: None,
        role: None,
        session_id: None,
        trust: Some(0.1),
        model_family: None,
        captured_at: None,
        supersedes: None,
    }
}

/// How many derived `Fact` nodes carry the `uses` predicate — the fact the deterministic
/// `uses` rule (confidence 0.8) produces from "X uses Y". Read straight from the store so the
/// assertion sees what the production consolidation path actually materialized.
fn uses_fact_count(memory: &Memory<FakeEmbedder>) -> usize {
    let query = BoundQuery::new("MATCH (f:Fact) WHERE f.predicate = $p RETURN f.id AS id")
        .bind_str("p", "uses")
        .expect("bind predicate");
    match memory
        .store()
        .execute(&query)
        .expect("uses-fact count query")
    {
        QueryResult::Rows(rows) => rows.row_count(),
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The total derived `Fact` count, so a test can assert "consolidation ran and derived
/// nothing" versus "consolidation derived a different fact".
fn total_fact_count(memory: &Memory<FakeEmbedder>) -> usize {
    let query = BoundQuery::new("MATCH (f:Fact) RETURN count(f) AS n");
    match memory.store().execute(&query).expect("fact count query") {
        QueryResult::Rows(rows) => match rows.value(0, 0) {
            Some(Value::Uint(n)) => usize::try_from(*n).unwrap_or(0),
            Some(Value::Int(n)) => usize::try_from(*n).unwrap_or(0),
            other => panic!("expected a count, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The deployment `[consolidation.extraction] min_confidence` floor is live through the
/// PRODUCTION consolidation path (`consolidate_tool` → `Memory::consolidate_once`), not just
/// inside the in-crate rule extractor. The tool builds the extractor itself from
/// `memory.pass_config().extraction`, so a raised floor must reach that extractor and silence
/// the 0.8 `uses` rule; with the parameterless `with_default_rules()` (the prior wiring) the
/// knob was inert and this test would still produce the fact.
///
/// "Alice uses Rust." matches only the `uses` rule (confidence 0.8). At the default floor
/// (0.8, a strict `<`) the rule fires and the `uses` fact is derived; raise the floor just
/// above 0.8 and the same episode through the same tool derives no fact at all.
#[tokio::test]
async fn consolidate_tool_min_confidence_floor_silences_the_uses_rule() -> TestResult {
    let agent = Id::generate();

    // Default floor (0.8): the 0.8 `uses` rule clears it, so the production path derives the
    // `uses` fact. This is the recall baseline the raised-floor run must differ from.
    let baseline = default_memory();
    capture_tool(
        &baseline,
        capture_params("Alice uses Rust.", &agent.to_string()),
        &now(),
        None,
        AuthEnabled(false),
    )
    .await?;
    let run = consolidate_tool(
        &baseline,
        ConsolidationRunToolParams {
            max_ticks: Some(3),
            verbose: Some(false),
        },
    )
    .await?;
    assert!(run.contains("consolidated=1"), "{run}");
    assert_eq!(
        uses_fact_count(&baseline),
        1,
        "at the default 0.8 floor the production path derives the `uses` fact"
    );

    // Raised floor (0.81 > 0.8): the SAME episode through the SAME tool must now derive no
    // `uses` fact. If the floor were inert (built from the hardcoded default), the count would
    // still be 1 — this is the assertion that pins the BLOCKER fix.
    let gated = memory_with_min_confidence(0.81);
    capture_tool(
        &gated,
        capture_params("Alice uses Rust.", &agent.to_string()),
        &now(),
        None,
        AuthEnabled(false),
    )
    .await?;
    let gated_run = consolidate_tool(
        &gated,
        ConsolidationRunToolParams {
            max_ticks: Some(3),
            verbose: Some(false),
        },
    )
    .await?;
    // The episode is still consolidated (it flips to done); it just yields no fact.
    assert!(gated_run.contains("consolidated=1"), "{gated_run}");
    assert_eq!(
        uses_fact_count(&gated),
        0,
        "the raised min_confidence floor reached the production extractor and silenced `uses`"
    );
    assert_eq!(
        total_fact_count(&gated),
        0,
        "no other rule fires on this episode, so the floor silences extraction entirely"
    );
    Ok(())
}
