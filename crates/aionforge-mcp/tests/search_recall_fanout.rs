//! Tests for the MCP `search` recall-shaping knobs: the `fanout` fan-out override (with its
//! MAX_FANOUT clamp) and the honest `considered` telemetry count. Hermetic — no transport, no
//! network; shares the `common` fake-embedder fixtures with the sibling MCP test binaries.
//!
//! The fake embedder maps every input to one shared vector, so dense scoring ties across all
//! candidates and lexical BM25 is the discriminator — exactly the namespace-scoped episode path
//! the recall fix governs. Distinct-text captures land as near-duplicates of one another but are
//! still stored, so a recall's episode scope is the full captured set.

mod common;

use std::sync::Arc;

use aionforge_domain::ids::Id;
use aionforge_engine::Memory;
use aionforge_mcp::{AuthEnabled, SearchToolParams, capture_tool, search_tool};

use common::{FakeEmbedder, capture_params, memory, now};

/// Parse the `hits: R of C considered | …` summary line into `(returned, considered)`.
fn summary_counts(out: &str) -> (usize, usize) {
    let rest = out
        .strip_prefix("hits: ")
        .unwrap_or_else(|| panic!("summary prefix: {out}"));
    let mut parts = rest.split_whitespace();
    let returned = parts
        .next()
        .expect("returned token")
        .parse()
        .expect("returned int");
    assert_eq!(parts.next(), Some("of"), "summary shape: {out}");
    let considered = parts
        .next()
        .expect("considered token")
        .parse()
        .expect("considered int");
    (returned, considered)
}

/// Capture `count` byte-distinct memories that all match the lexical query "telemetry", in one
/// agent's private namespace — so a recall's episode scope is exactly this set.
async fn capture_many_matching(memory: &Arc<Memory<FakeEmbedder>>, agent: &Id, count: usize) {
    for i in 0..count {
        capture_tool(
            memory,
            capture_params(
                &format!(
                    "telemetry pipeline note {i}: the recall served size metric is recorded at \
                     the single render seam so an operator can chart observability over time"
                ),
                &agent.to_string(),
            ),
            &now(),
            None,
            AuthEnabled(false),
        )
        .await
        .expect("capture");
    }
}

/// Run a `search` for "telemetry" as `agent`, with explicit fan-out and limit knobs.
async fn search_telemetry(
    memory: &Arc<Memory<FakeEmbedder>>,
    agent: &Id,
    fanout: Option<usize>,
    limit: Option<usize>,
) -> String {
    search_tool(
        memory,
        SearchToolParams {
            query: "telemetry".to_string(),
            viewer: Some(format!("agent:{agent}")),
            principal: None,
            teams: Vec::new(),
            limit,
            verbose: None,
            include_superseded: None,
            fanout,
            min_relevance: None,
        },
        &now(),
        None,
        AuthEnabled(false),
    )
    .await
    .expect("search")
}

#[tokio::test]
async fn search_tool_fanout_knob_widens_recall_and_clamps_safely() {
    let memory = memory();
    let agent = Id::generate();
    capture_many_matching(&memory, &agent, 20).await;

    // Hold the limit small and constant so the only variable is the fan-out. The returned
    // bundle is trimmed to the limit either way (the fan-out is floored to the limit, so it
    // can never under-fill it); the knob's real job is how DEEP a pool fusion ranks — the
    // recall ceiling. A narrow fan-out ranks only a shallow pool of the 20 matching memories,
    // so a relevant memory past the ceiling is never seen.
    let (_, narrow) = summary_counts(&search_telemetry(&memory, &agent, Some(3), Some(3)).await);
    // A wide fan-out spends the budget across the reader's in-scope episodes, so the whole
    // matching set is considered even though the returned bundle stays the same size.
    let (_, wide) = summary_counts(&search_telemetry(&memory, &agent, Some(50), Some(3)).await);
    assert!(
        wide > narrow,
        "a wider fan-out deepens the considered pool: narrow={narrow} wide={wide}"
    );

    // A fan-out far past MAX_FANOUT is clamped, not rejected and not run unbounded: the query
    // succeeds and the considered pool matches the wide-but-in-range run, proving the cap is a
    // safe ceiling rather than a recall regression. (The corpus is far under the 1000 ceiling,
    // so both runs see the same pool; the point is the over-cap request is handled safely.)
    let (_, clamped) =
        summary_counts(&search_telemetry(&memory, &agent, Some(10_000_000), Some(3)).await);
    assert_eq!(
        clamped, wide,
        "an over-cap fan-out clamps to the ceiling and preserves recall: clamped={clamped} wide={wide}"
    );
}

#[tokio::test]
async fn search_tool_considered_count_reports_the_fused_pool_not_the_returned_count() {
    let memory = memory();
    let agent = Id::generate();
    capture_many_matching(&memory, &agent, 12).await;

    // A wide fan-out with a small limit: fusion ranks the whole matching pool, but the bundle
    // is trimmed to the limit. The `considered` telemetry must report the pool it ranked, not
    // echo the trimmed returned count — otherwise a too-narrow recall (a small considered
    // count) would be indistinguishable from a deliberately small limit, and the recall-
    // attrition gap an operator needs to see would be hidden (03 §6).
    let out = search_telemetry(&memory, &agent, Some(50), Some(3)).await;
    let (returned, considered) = summary_counts(&out);
    assert_eq!(returned, 3, "the bundle is trimmed to the limit: {out}");
    assert!(
        considered >= 12,
        "considered reports the fused pool ({considered}), strictly more than returned \
         ({returned}): {out}"
    );
}

#[tokio::test]
async fn search_tool_rejects_explicit_zero_fanout() {
    let memory = memory();
    let agent = Id::generate();
    capture_many_matching(&memory, &agent, 4).await;

    // `fanout = Some(0)` is a caller error, not a second spelling of the default: omitting
    // `fanout` already maps to the deployment default (the internal `0` sentinel), so an
    // explicit `0` must be rejected rather than silently absorbed — which would hide the
    // mistake and make `Some(0)` indistinguishable from `None`. Call `search_tool` directly
    // (not the `search_telemetry` helper) so the `Err` is inspectable.
    let err = search_tool(
        &memory,
        SearchToolParams {
            query: "telemetry".to_string(),
            viewer: Some(format!("agent:{agent}")),
            principal: None,
            teams: Vec::new(),
            limit: Some(3),
            verbose: None,
            include_superseded: None,
            fanout: Some(0),
            min_relevance: None,
        },
        &now(),
        None,
        AuthEnabled(false),
    )
    .await
    .expect_err("explicit fanout=0 must be rejected");
    assert!(
        err.starts_with("ERR_INVALID_FANOUT"),
        "fanout=0 returns a typed validation error: {err}"
    );

    // Omitting `fanout` still succeeds and uses the deployment default — only the explicit
    // `0` is rejected, never the absence of the knob.
    let (_, considered) = summary_counts(&search_telemetry(&memory, &agent, None, Some(3)).await);
    assert!(
        considered >= 4,
        "omitting fanout uses the deployment default and still considers the in-scope pool: \
         considered={considered}"
    );
}

/// Build search params for "telemetry" as `agent` with an explicit `min_relevance`.
fn search_params_with_floor(agent: &Id, min_relevance: Option<f64>) -> SearchToolParams {
    SearchToolParams {
        query: "telemetry".to_string(),
        viewer: Some(format!("agent:{agent}")),
        principal: None,
        teams: Vec::new(),
        limit: Some(3),
        verbose: None,
        include_superseded: None,
        fanout: None,
        min_relevance,
    }
}

#[tokio::test]
async fn search_rejects_out_of_range_min_relevance() {
    let memory = memory();
    let agent = Id::generate();
    capture_many_matching(&memory, &agent, 4).await;

    // An out-of-range floor is a caller error, not a silently-clamped value: omitting
    // `min_relevance` already maps to the deployment default, so `2.0` (or a negative) must be
    // rejected with a typed error rather than quietly emptying every recall (P0a). Mirrors the
    // fanout==0 rejection.
    let err = search_tool(
        &memory,
        search_params_with_floor(&agent, Some(2.0)),
        &now(),
        None,
        AuthEnabled(false),
    )
    .await
    .expect_err("an out-of-range min_relevance must be rejected");
    assert!(
        err.starts_with("ERR_INVALID_MIN_RELEVANCE"),
        "min_relevance=2.0 returns a typed validation error: {err}"
    );

    // An in-range floor of 0.0 (off) succeeds and behaves like omitting it: the fake embedder
    // ties every dense score, so a 0.0 floor never drops a hit and the in-scope pool is intact.
    let out = search_tool(
        &memory,
        search_params_with_floor(&agent, Some(0.0)),
        &now(),
        None,
        AuthEnabled(false),
    )
    .await
    .expect("an in-range floor of 0.0 is accepted");
    let (_, considered) = summary_counts(&out);
    assert!(
        considered >= 4,
        "a 0.0 floor leaves recall byte-identical to the default: considered={considered}"
    );
}
