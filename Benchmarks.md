# Aionforge Memory Benchmarks

Living ledger of Aionforge Memory benchmark/eval results. The implementor updates
this in the same PR whenever a change activates or tunes a retrieval or
consolidation parameter from a benchmark, adds or changes a benchmark, or produces
new measured results. Each entry records the date, PR/commit, benchmark and
fixture, metrics, result, and the decision it informed.

Benchmarks are `#[ignore]` / local-on-demand unless noted otherwise. They are not
run in CI.

## Retrieval Signal Sweeps

Graph-bearing benchmark, in-process store, fake deterministic embedder:
`cargo test -p aionforge-eval --test graph_bearing_bench -- --ignored --nocapture`

### R2 - Louvain Community-Diversity Cap

Date: 2026-06-17/18  
Benchmark PR: [#291](https://github.com/jscott3201/aionforge-memory/pull/291)  
Activation PR: [#293](https://github.com/jscott3201/aionforge-memory/pull/293),
merge commit `b97509b`  
Fixture: query `project work`, limit 3, one dominant four-fact Louvain community
(`Quinn`) versus two diverse one-offs (`Rosa`, `Sam`)  
Metrics: diverse recall@3, redundancy@3

| cap | diverse recall@3 | redundancy@3 |
|---:|---:|---:|
| 0 (off) | 0.000 | 0.667 |
| 1 | 1.000 | 0.000 |
| 2 (activated, #293) | 0.500 | 0.333 |
| 3 | 0.000 | 0.667 |

Decision: activated at cap 2 as a conservative first activation. It breaks the
one-cluster wall while keeping up to two facts from a community, so legitimate
single-entity recall is not gutted. Cap 1 was diversity-optimal on the fixture
but too aggressive for a first production default; cap 3 was inert.

### R1 - Global PageRank Authority Prior

Date: 2026-06-17/18  
Benchmark PR: [#291](https://github.com/jscott3201/aionforge-memory/pull/291)  
Decision state: staged off, authority weight 0  
Metrics: recall@4, nDCG@4

Disconnected fixture: query `aionforge contributors`, four hub-gold facts sharing
the `Aionforge` subject versus four peripheral one-off project facts, dense-equal
at topic 0.

Command:
`cargo test -p aionforge-eval --test graph_bearing_bench authority_weight_sweep -- --ignored --nocapture`

| authority weight | recall@4 | nDCG@4 |
|---:|---:|---:|
| off | 0.750 | 0.805 |
| 0.3 | 0.750 | 0.805 |
| 0.6 | 0.750 | 0.805 |
| 1.0 | 0.750 | 0.805 |
| 2.0 | 0.750 | 0.805 |

Connected fixture: query `connected project hub`, four hub-gold facts sharing
`ABOUT -> Aionforge` plus extra `SUPPORTS` degree versus four peripheral
distractors sharing only the source episode, dense-equal at topic 4. The sweep
uses `MultiHop` + `TemporalMode::History` to isolate authority from the
Current-only Support expansion.

Command:
`cargo test -p aionforge-eval --test graph_bearing_bench connected_authority_weight_sweep -- --ignored --nocapture`

| authority weight | recall@4 | nDCG@4 |
|---:|---:|---:|
| off | 0.750 | 0.832 |
| 0.3 | 0.750 | 0.832 |
| 0.6 | 0.750 | 0.832 |
| 1.0 | 0.750 | 0.832 |
| 2.0 | 1.000 | 1.000 |

Finding: authority lift is real but narrow. It provides effectively zero lift on
a disconnected graph because undirected PageRank concentrates restart mass in
small components, letting peripheral islands outrank hubs. On a connected graph,
the lift is a step function: flat through weight 1.0, then snapping to perfect
recall/nDCG at weight 2.0 because authority is RRF-fused against Dense, Graph,
and Trust at lower weights. On the default Current path, Support expansion
already recovers the reinforced hub facts, so authority's marginal value there is
small.

Decision: keep R1 staged off. The R1 activation follow-up must gate authority to
connected graphs and non-Current modes rather than applying a flat global flip.

## Memory Benchmark Harness

BEAM north-star / LongMemEval A/B, local on-demand:

Populated as the eval track lands: ingest adapter (BRIEF-6) -> LongMemEval
Recall@k/nDCG@k/MRR scorer -> LoCoMo/BEIR smokes -> BEAM 128K+ tiers
(cost-gated).
