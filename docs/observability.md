# Observability

Aionforge emits spans/events through the [`tracing`](https://docs.rs/tracing) facade and metrics
through the `metrics` facade. The **`aionforge` binary installs a tracing subscriber** (see
[Logging](#logging) below), so events reach stderr out of the box; the **metrics** facade stays a
no-op until a host installs a recorder (a deliberate follow-up wired with the operator console).
Metric labels and span fields are deliberately low-cardinality: no query text, memory content,
namespace ids, agent ids, file paths, request ids, or model names are used. Use audit reads and
`aionforge doctor --json` for high-detail inspection.

## Logging

The library crates only *emit* through `tracing`; the binary owns the **subscriber** (the sink).
It is installed once in `main`, before any subcommand runs, so `serve`, `doctor`, and `recover`
are all covered.

```sh
aionforge serve http                     # text on stderr, level `info` (defaults)
aionforge serve http --log-format json   # structured JSON (or AIONFORGE_LOG_FORMAT=json)
RUST_LOG=aionforge_store=debug,aionforge_mcp=info aionforge doctor
```

- **Format** — `--log-format text|json` is a global flag. Precedence: flag → `AIONFORGE_LOG_FORMAT`
  env → default `text`. An unparseable value falls back to the default; logging setup never fails
  the process.
- **Level/filter** — standard `tracing` `EnvFilter` via `RUST_LOG` (default `info`).
- **stderr only (invariant)** — the subscriber writes to **stderr**. The MCP **stdio** transport
  owns **stdout** for the JSON-RPC protocol, so logging there would corrupt the stream. CLI report
  output (`doctor`/`recover`) stays on stdout; diagnostics stay on stderr.

### Levels

| Level | Use for |
|---|---|
| `error` | A failure the operator must act on. |
| `warn` | Degraded/misconfigured but still serving (auth health, unanchored writer, stdio-auth-unsupported, link-evolution skips). |
| `info` | Lifecycle: startup embedder check, auth posture, shutdown, index-kind reconciliation, the traffic heartbeat. |
| `debug` | Per-request detail: recall served, induction gates, discovery anomalies. |
| `trace` | Deep diagnostics; off by default. |

Events carry an explicit `target:` (`aionforge::serve`, `aionforge::traffic`, `aionforge_mcp::auth`,
`aionforge_mcp::telemetry`, …) so `RUST_LOG` can filter by subsystem; library events without one
inherit their module path (`aionforge_store::…`).

### PII / secret hygiene — the load-bearing rule

This is a **memory store**: the wrong log line is a data leak.

> **Log identifiers, kinds, counts, latencies, and outcomes — never memory content, embeddings,
> tokens, keys, or raw claim values.**

Log a memory by its `id`, never its `content`/`statement`; log sizes/estimates, never the bytes
themselves; auth advisories name only the non-secret issuer **origin**. A CI gate
(`.github/scripts/check-no-log-leakage.sh`) is a tripwire against interpolating a sensitive field
into a `tracing`/`log` macro; a genuinely-safe flagged line can carry a `// log-leak-ok`
justification, but for `content`/`embedding` there should be none. The gate is line-based, so it
cannot catch a field split across the lines of a multi-line macro, nor one pre-formatted into a
string and logged later (`let m = format!("{}", x.secret); info!("{m}")`). **Don't pre-format
sensitive fields into log strings** — pass structured `tracing` fields directly so the gate (and a
reviewer) can see them. Real safety rests on this convention plus review; the gate is the tripwire.

## Traffic heartbeat & token estimation

The server logs a periodic in/out **traffic heartbeat** at `info` on the `aionforge::traffic`
target (default every 5 minutes), plus a final summary (`phase=shutdown`) on graceful shutdown:

```
INFO aionforge::traffic: memory traffic phase=heartbeat
     bytes_in_total=… bytes_out_total=… bytes_in_delta=… bytes_out_delta=…
     est_tokens_in_total=… est_tokens_out_total=… est_tokens_in_delta=… est_tokens_out_delta=…
```

- **IN** = `content` bytes clients push via `capture`/`batch_capture` (memory text being stored).
  **OUT** = rendered recall responses of `search`/`read_memory`/`session_manifest`/the `work_*`
  readers (the dominant outbound payload). Small control traffic (query params, receipts) is not
  counted — this is a memory-throughput signal, not a wire-level byte meter. Counts are
  process-cumulative and reset on restart; only HTTP/stdio tool traffic is counted.
- **Bytes are authoritative; tokens are an estimate.** The server cannot run the calling client's
  tokenizer, so `est_tokens` is a deliberately coarse proxy — **bytes ÷ 4** (≈4 characters/token).
  Use it for order-of-magnitude capacity/cost intuition, never as an exact count or billing source.
  The same divisor backs the per-recall `est_tokens` debug line in `aionforge_mcp::telemetry`.
- **Cadence** — `AIONFORGE_TRAFFIC_HEARTBEAT_SECS` (whole seconds; `0` disables; unset → 300).

## Tracing

Trace spans cover the MCP tool-call boundary plus the capture, recall, and
consolidation pipeline. They use stable operation names and bounded fields only:

| Span | Fields | Meaning |
|---|---|---|
| `aionforge.mcp.tool` | `tool`, `authenticated`, `outcome`, `error`, `latency_ms` | One MCP tool call, wrapping every tool at the dispatch choke point (the per-area spans below nest under it). `tool` is the low-cardinality tool name; `authenticated` is a bool for whether a validated principal rode the request — never an agent/session id, the arguments, or the response body. `error` is `none`, `tool_error` (a tool's own error result), or `dispatch_error` (an rmcp-level failure such as an unknown tool). |
| `aionforge.capture` | `role`, `namespace`, `trusted`, `signed`, `outcome`, `verdict`, `embedding`, `error` | One capture request. `namespace` is the namespace kind (`agent`, `team`, `global`, `system`), never the namespace id. |
| `aionforge.capture.stage` | `stage` | Fixed capture stages: `filter`, `embed`, and `commit`. |
| `aionforge.recall` | `class`, `temporal`, `sensitive`, `include_expired`, `include_system`, `mode_override`, `deadline`, `fanout`, `limit`, `outcome`, `embedder`, `error`, `returned`, `candidates_considered`, `signals_run` | One recall request. The query text and principal id are never fields. |
| `aionforge.recall.stage` | `stage` | Fixed recall stages, currently `classify` and `assemble`. |
| `aionforge.recall.signal` | `signal`, `fanout` | Signal-level work for `query_embed`, `lexical`, `lexical_anchor`, `dense`, `support`, `graph`, `trust`, `importance`, and `recency`. |
| `aionforge.consolidation.tick` | `batch_size`, `outcome`, `error`, `consolidated`, `retried`, `failed`, `pending_after` | One foreground or background consolidation tick. |
| `aionforge.consolidation.episode` | `role`, `namespace`, `state`, `outcome`, `error` | One episode processed inside a tick. The episode id and content are never fields. |
| `aionforge.consolidation.pass` | `pass`, `version`, `outcome`, `error` | One enabled consolidation pass applied to one episode. Pass names are stable rule identifiers from the registered pass set. |

Error fields reuse the metric vocabulary where possible: capture errors use
`filter`, `store`, `unauthorized`, `invalid_signature`, `clock_skew`,
`provenance_unavailable`, or `system_role_not_writable`; recall errors use `store`
or `deadline_exceeded`; consolidation tick errors use `store` or `timeout`; pass
errors use `transient` or `fatal`.

The store lifecycle also emits events on the `aionforge::store` target (complementing
the `aionforge_store_open_*` metrics): `store opened` / `store open failed`
(`mode` = `fresh`|`recover`, `outcome`, `elapsed_ms`) and, from `migrate` (previously
silent), `schema migrated` at `info` (`from_version`, `to_version`, `applied` type
count) when migrations run, plus a `debug` no-op event (`from_version`) when the
schema is already current. All fields are low-cardinality integers/labels; no path
or data.

## Capture

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `aionforge_capture_requests_total` | counter | `outcome`, `verdict`, `embedding`, `error` | Capture attempts by bounded result class. Success uses `error="none"`; errors use `verdict="none"` and `embedding="none"`. |
| `aionforge_capture_duration_seconds` | histogram | `outcome`, `verdict`, `embedding`, `error` | End-to-end capture latency at the engine facade. |
| `aionforge_capture_redactions_total` | counter | none | Redactions applied by the capture privacy filter. |
| `capture_injection_marker_hits_total` | counter | `marker` | Injection marker hits reported by the capture filter. Marker ids come from the configured detector set, not user text. |

Capture error labels are bounded to `filter`, `store`, `unauthorized`,
`invalid_signature`, `clock_skew`, `provenance_unavailable`, and
`system_role_not_writable`, with `other` reserved for future capture error variants.

## Recall

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `aionforge_recall_requests_total` | counter | `outcome`, `class`, `embedder`, `error` | Recall attempts by query class and embedder availability. Error paths use `class="unknown"` and `embedder="unknown"`. |
| `aionforge_recall_duration_seconds` | histogram | `outcome`, `class`, `embedder`, `error` | End-to-end recall latency, including explicit namespace-denial audit checks. |
| `aionforge_recall_candidates_considered` | histogram | `class` | Authorized candidates considered after fusion/filtering. |
| `aionforge_recall_entries_returned` | histogram | `class` | Structured entries returned in the recall bundle. |
| `aionforge_recall_stage_duration_seconds` | histogram | `class`, `stage` | Internal recall stage timings for `classify`, `signals`, and `assemble`. |

Recall error labels are `audit`, `store`, `deadline_exceeded`, and `other`.

## Consolidation

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `consolidation_ticks_total` | counter | `outcome`, `error` | Background or foreground scheduler ticks. Success uses `error="none"`; errors are `store` or `timeout`. |
| `consolidation_tick_duration_seconds` | histogram | `outcome`, `error` | One scheduler tick duration. |
| `consolidation_episodes_consolidated_total` | counter | none | Episodes consolidated by scheduler ticks. |
| `consolidation_episodes_retried_total` | counter | none | Episodes left raw for retry after transient pass failure. |
| `consolidation_episodes_failed_total` | counter | none | Episodes marked failed by the scheduler. |
| `consolidation_recovery_resets_total` | counter | none | `in_progress` episodes reset to `raw` at consolidator startup. |
| `consolidation_lag_seconds` | gauge | none | Age since `ingested_at` for the oldest pending episode. |
| `consolidation_episodes_pending` | gauge | none | Pending consolidation backlog size. |
| `consolidation_episodes_failed` | gauge | none | Failed episode count. |
| `consolidation_supersessions_total` | counter | none | Supersession decisions materialized by fact extraction. |
| `consolidation_contradictions_total` | counter | none | Contradiction decisions observed by fact extraction. |
| `consolidation_quarantines_total` | counter | none | Quarantines materialized by fact extraction. |
| `consolidation_summaries_total` | counter | none | Summary notes written by fact extraction. |

## Link Evolution And Guard

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `aionforge_link_evolution_runs_total` | counter | `outcome` | Link-evolution runs by success/error. |
| `aionforge_link_evolution_duration_seconds` | histogram | `outcome` | Link-evolution run duration. |
| `aionforge_link_evolution_links_created_total` | counter | none | New `RELATES_TO` links opened. |
| `aionforge_link_evolution_links_revised_total` | counter | none | Existing links revised. |
| `aionforge_link_evolution_declined_total` | counter | none | Evolver calls declined or unavailable. |
| `aionforge_consolidation_guard_refusals_total` | counter | `surface` | Cross-family guard refusals, surfaced for `link_evolve`. |
| `aionforge_startup_warnings_total` | counter | `kind` | Engine construction warnings, currently `single_family_deployment`. |

Warn-mode cross-family findings are recorded in the audit trail. The public reports only
carry refused counts, so metrics expose refusals and startup warnings without inferring
warn-mode totals.

## Maintenance And Recovery

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `aionforge_store_open_total` | counter | `mode`, `outcome` | Durable store opens. `mode` is `fresh` or `recover`. |
| `aionforge_store_open_duration_seconds` | histogram | `mode`, `outcome` | Store open/recovery duration. |
| `aionforge_forgetting_sweeps_total` | counter | `outcome` | Forgetting sweep pages by `success`, `error`, or `disabled`. |
| `aionforge_forgetting_sweep_duration_seconds` | histogram | `outcome` | Forgetting sweep duration. |
| `aionforge_forgetting_candidates_scanned` | histogram | none | Candidates scanned by a forgetting page. |
| `aionforge_forgetting_memories_forgotten_total` | counter | none | Memories soft-forgotten by sweeps. |
| `aionforge_forgetting_memories_spared_total` | counter | none | Candidates spared by the forgetting gate. |
| `aionforge_drift_sweeps_total` | counter | `surface`, `outcome` | Drift and cooling sweep pages by result. |
| `aionforge_drift_sweep_duration_seconds` | histogram | `surface`, `outcome` | Drift/cooling sweep duration. |
| `aionforge_drift_blocks_scanned` | histogram | none | Core blocks scanned by drift detection. |
| `aionforge_drift_warnings_emitted_total` | counter | none | New drift warning audit rows. |
| `aionforge_drift_max_score` | gauge | none | Highest drift score observed on a sweep page. |
| `aionforge_cooling_facts_scanned` | histogram | none | Facts scanned by cooling sweeps. |
| `aionforge_cooling_facts_cooled_total` | counter | none | Facts newly stamped with cooling windows. |

## Doctor And Capacity

`Memory::doctor_report()` and `aionforge doctor` emit capacity gauges from the same
canonical snapshot they return:

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `aionforge_graph_nodes` | gauge | none | Live graph node count from the doctor capacity report. |
| `aionforge_graph_edges` | gauge | none | Live graph edge count from the doctor capacity report. |
| `aionforge_graph_generation` | gauge | none | Current graph generation. |
| `aionforge_doctor_ok` | gauge | none | `1` when the full engine doctor report is healthy, else `0`. |
