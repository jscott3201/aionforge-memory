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

### Off-Topic Rejection Floor

Date: 2026-06-18

Benchmark PR: pending (BRIEF-10)

Runner:
`set -a && source ~/.aionforge/aionforge-redeploy.env && set +a && cargo test -p aionforge-eval --test floor_sweep -- --ignored --nocapture`

Fixture: synthetic `aionforge-eval` floor corpus, 21 memories and 24 queries
(10 positive, 14 off-topic negatives). Negatives include far off-topic everyday
questions plus adjacent-but-off-topic technical questions.

Metrics: rejection_rate over off-topic negatives, false_rejection_rate over
positive queries, Recall@5, nDCG@5. One cached production embedder pass used
OpenRouter `google/gemini-embedding-2`, approximately 937 input tokens, and
estimated embedding spend of `$0.0001` at `$0.1500` per 1M tokens.

| uniform min_relevance | rejection_rate | false_rejection_rate | recall@5 | nDCG@5 |
|---:|---:|---:|---:|---:|
| 0.00 | 0.000 | 0.000 | 0.950 | 0.921 |
| 0.35 | 0.000 | 0.000 | 0.950 | 0.921 |
| 0.40 | 0.000 | 0.000 | 0.950 | 0.921 |
| 0.45 | 0.000 | 0.000 | 0.950 | 0.921 |
| 0.50 | 0.071 | 0.000 | 0.950 | 0.921 |
| 0.55 | 0.643 | 0.000 | 0.950 | 0.921 |
| 0.60 | 0.929 | 0.000 | 1.000 | 0.929 |
| 0.62 | 1.000 | 0.000 | 1.000 | 0.930 |
| 0.65 | 1.000 | 0.000 | 0.950 | 0.923 |
| 0.70 | 1.000 | 0.100 | 0.750 | 0.809 |

| arm | floor source | rejection_rate | false_rejection_rate | recall@5 | nDCG@5 |
|---|---|---:|---:|---:|---:|
| floor off | forced 0.00 | 0.000 | 0.000 | 0.950 | 0.921 |
| shipped profile | router defaults | 0.929 | 0.000 | 1.000 | 0.929 |

Decision: keep the already-shipped 0.60 per-class dense floors as the
conservative production profile documented by this harness. A uniform 0.62 row
rejected all negatives at zero false rejection on this broadened fixture, but
BRIEF-10 records rather than retunes; 0.60 rejects 13/14 negatives, keeps
false_rejection_rate at 0.000, and improves positive recall@5 by de-cluttering
relative to floor-off. DBSF and magnitude-aware fusion arms remain deferred.

### LongMemEval_S Retrieval A/B

Date: 2026-06-18

Benchmark PR: pending (BRIEF-9)

Runner:
`AIONFORGE_LONGMEMEVAL_LIMIT=30 cargo test -p aionforge-eval --release --test longmemeval_scorer longmemeval_s_real_embedder -- --ignored --nocapture`

Fixture: external LongMemEval_S via `AIONFORGE_LONGMEMEVAL_DATA` or
`~/.aionforge/longmemeval-data/LongMemEval_S.json`

Metrics: Recall@k, nDCG@k, MRR. First run used 30/500 questions,
per-question haystack seeding, turn-level gold from `has_answer`, k=10,
approximately 3,581,009 input tokens, and estimated embedding spend of
`$0.5372` at `$0.1500` per 1M tokens.

| arm | fixture | k | questions | recall@k | nDCG@k | MRR | status |
|---|---|---:|---:|---:|---:|---:|---|
| RRF default | LongMemEval_S, first 30 questions, turn gold | 10 | 30 | 0.917 | 0.819 | 0.802 | measured locally with OpenRouter `google/gemini-embedding-2` |

Decision: first real labeled retrieval baseline established. Use this row as the
RRF-default reference for the follow-on FusionStrategy A/B arms.
