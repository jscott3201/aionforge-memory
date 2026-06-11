# Observability

Aionforge emits metrics through the `metrics` facade. If a host installs no recorder,
metric calls are no-ops. Metric labels are deliberately low-cardinality: no query text,
memory content, namespace ids, agent ids, file paths, or model names are used as labels.
Use audit reads and `aionforge doctor --json` for high-detail inspection.

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
| `consolidation_lag_seconds` | gauge | none | Age of the oldest pending episode. |
| `consolidation_episodes_pending` | gauge | none | Pending consolidation backlog size. |
| `consolidation_episodes_failed` | gauge | none | Failed episode count. |
| `consolidation_supersessions_total` | counter | none | Supersession decisions materialized by fact extraction. |
| `consolidation_contradictions_total` | counter | none | Contradiction decisions observed by fact extraction. |
| `consolidation_quarantines_total` | counter | none | Quarantines materialized by fact extraction. |
| `consolidation_summaries_total` | counter | none | Summary notes written by fact extraction. |

## Optional LLM Layers And Guard

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `aionforge_distillation_runs_total` | counter | `outcome` | Distillation runs by success/error. |
| `aionforge_distillation_duration_seconds` | histogram | `outcome` | Distillation run duration. |
| `aionforge_distillation_notes_written_total` | counter | none | Distilled notes written. |
| `aionforge_distillation_declined_total` | counter | none | Model calls declined or unavailable. |
| `aionforge_distillation_rejected_lossy_total` | counter | none | Summaries rejected by the detail-retention guard. |
| `aionforge_link_evolution_runs_total` | counter | `outcome` | Link-evolution runs by success/error. |
| `aionforge_link_evolution_duration_seconds` | histogram | `outcome` | Link-evolution run duration. |
| `aionforge_link_evolution_links_created_total` | counter | none | New `RELATES_TO` links opened. |
| `aionforge_link_evolution_links_revised_total` | counter | none | Existing links revised. |
| `aionforge_link_evolution_declined_total` | counter | none | Model calls declined or unavailable. |
| `aionforge_consolidation_guard_refusals_total` | counter | `surface` | Cross-family guard refusals for `distill` or `link_evolve`. |
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
