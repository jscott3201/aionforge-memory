//! Response-size telemetry for the recall-serving MCP tools, through the workspace
//! `metrics` 0.24 facade.
//!
//! This is pure observation at the recall-serve boundary: the realized byte size of every
//! memory-bearing response (`search`, `read_memory`, `session_manifest`) is folded into one
//! labeled counter and echoed on a `tracing` line, so an operator can measure how many bytes
//! the store actually hands back per tool and per session. It changes no behavior and reads
//! nothing back — the [`Authorizer`](aionforge_engine), the visible set, and ranking are
//! untouched.
//!
//! Bytes, not tokens, are the metric. The server cannot run the calling client's tokenizer,
//! so an exact token count is not free; bytes are the authoritative, stable measure (the same
//! quantity the store-vs-file savings estimate ultimately rests on). The token figure is only
//! ever a clearly-labeled estimate from a coarse, documented divisor on the `tracing` line —
//! never a scraped metric and never presented as exact. A real per-token estimate (and the
//! per-session `server_status` rollup) is a deliberate follow-up.
//!
//! Cost is ~nothing until an operator installs a recorder: the `metrics` facade is a no-op
//! against the default `NoopRecorder`, so these instruments add no always-on overhead.

/// Record the realized served size of a recall-like response: fold the response's byte length
/// into the per-tool bytes-served counter (the authoritative measure) and emit a per-request
/// `tracing` size line carrying the byte count and a labeled token estimate.
///
/// `tool` is the low-cardinality MCP tool name (e.g. `search`); `response` is the exact string
/// the tool hands back to the client, measured after rendering so the count is authoritative
/// and counted in exactly one place per response.
pub(crate) fn record_recall_served(tool: &'static str, response: &str) {
    let bytes = response.len() as u64;
    ::metrics::counter!("aionforge_mcp_recall_bytes_served_total", "tool" => tool).increment(bytes);
    // Fold into the process-global OUT total that the periodic traffic heartbeat reports.
    crate::traffic::record_out(bytes);
    ::tracing::debug!(
        target: "aionforge_mcp::telemetry",
        tool,
        response_bytes = bytes,
        est_tokens = bytes / crate::traffic::TOKEN_ESTIMATE_BYTES_PER_TOKEN,
        "recall served"
    );
}
