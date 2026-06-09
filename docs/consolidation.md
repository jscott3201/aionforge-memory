# Consolidation

Consolidation is the slow, asynchronous side of memory. Capture writes a raw episode and returns;
some time later, a background worker reads that episode and derives the durable knowledge from it —
the facts, the entities, the contradictions, the summaries. The two halves are deliberately split.
Capture stays on a fast, narrow path; the expensive thinking happens off to the side, on its own
schedule, where it can take its time and recover from a crash without ever holding up a write.

The whole of it is deterministic. Given the same episode and the same rule versions, a consolidation
pass derives the same facts with the same ids, byte for byte, every time it runs — which is what
makes a re-run after a crash a no-op rather than a second copy. This page describes the canonical,
deterministic pipeline. The optional LLM [distillation](distillation.md) and
[link-evolution](link-evolution.md) layers run *outside* this pipeline, against the
already-committed facts and in their own transactions; they are documented separately and never touch
the cursor described here.

## The cursor and the work queue

Two pieces of durable state drive the scheduler. The store holds a single
`ConsolidationCursor` — a watermark over the commit stream — and every episode carries a
`consolidation_state`: `raw`, `in_progress`, `consolidated`, or `failed`. The cursor is the
resume point; the per-episode state is the work queue. Together they give resume-not-reprocess across
a restart and an idempotency no crash can turn into a double-apply.

The cursor also records the `{pass_name: version}` map in force at its position. A version bump on a
pass is the signal to reprocess; the engine records it so a later milestone can act on it.

## The tick

`Consolidator::tick_once` is the unit of work and the deterministic test seam. One tick:

1. Discovers a bounded batch of pending episodes in commit order (`batch_size`, default 32 — the
   per-tick concurrency bound).
2. For each episode, marks it `in_progress`, runs every enabled `ConsolidationPass` over a
   read snapshot, and accumulates each pass's derived output.
3. On success, materializes that merged output, flips the episode to `consolidated`, and advances
   the cursor — all in **one atomic commit**.

Episodes are processed one at a time, and a tick **stops at the first episode that does not
consolidate**. The cursor tracks the contiguous consolidated prefix, so it must never jump past a
held-back failure: the skipped tail stays pending and is rediscovered, in order, next tick. A
`Consolidator::start` spawns this loop on a timer (`tick_interval`, default 5s) and hands back a
`ConsolidationHandle` that shuts it down gracefully, letting the in-flight tick finish.

### Why the pass is read-only

A `ConsolidationPass` reads a snapshot and returns a `PassOutput`; it must **not** open a write
transaction. That is a deliberate constraint, not a convenience. Every write is the scheduler's, co-
committed atomically with the episode's state-flip. That single rule is what lets a crash mid-pass
resume cleanly: because nothing a pass does touches the graph until the flip, an interrupted pass
leaves the episode exactly as it found it — `raw` — to be re-run whole, never half-applied. The
output type is literally the store's `ConsolidationArtifacts`, re-exported under the seam name, so a
pass's return value *is* what the commit writes, with no copy step in between.

### Crash safety and retries

If the worker dies mid-tick, at most the one in-flight episode is affected, and it is sitting at
`in_progress`. On the next startup, `reset_in_progress_episodes` returns any such episode to `raw`,
so the next tick re-runs it from scratch. Because the derived ids are content-addressed (see below),
that re-run reconstructs the identical artifacts and the flip writes nothing new.

A pass failure is classified, not fatal to the pipeline. A `PassError::Transient` (a down embedder,
a rate limit, a timeout) leaves the episode `raw` so it is the oldest pending next tick — the cursor
genuinely holds at it until it succeeds. A `PassError::Fatal`, or a transient failure that exceeds
`max_retries` (default 5), marks the episode `failed`: it is retained and audited, excluded from
the queue, and later ticks proceed past it. A poison-pill episode awaits an operator's reconcile or
skip rather than wedging the whole pipeline. The attempt count is read from the durable
`consolidation_failed` audit trail, not held in memory, so a crash does not hand a failing episode a
fresh retry budget on every restart.

## Fact extraction

The `extract_facts` pass is the first rule that derives memory. It runs an injected `FactExtractor`
over the episode, resolves the entities, derives the facts, detects conflicts against the current
set, and conservatively summarizes — all from a read-only snapshot.

The shipped extractor (`RuleExtractor`) is deterministic and pattern-based: it scans episode text
for a small set of verb-marker relations (`works on`, `is based in`, `prefers`, `uses`, `is a`) and
emits one `(subject, predicate, object)` triple per match, recording the matched sentence's byte
range as a `SourceSpan`. The model-backed extractor is a later, optional substitution behind the same
seam. Each derived `Fact` carries its triple, a `confidence` from the matched rule, its statement
text, and an `Extraction` block of provenance: the extractor's model family/version, the source
spans, and the rule version. Every resolution decision is recorded as a `canonicalize` audit event.

The fact id is a content hash over the canonical triple, the source episode, and the rule version:

```rust
fn fact_id(namespace, subject_id, predicate, object, episode_id, rule_version) -> Id
```

Re-extracting an episode yields the same id, which is exactly what makes re-extraction idempotent.

## Entity resolution

Before a fact is built, every subject and entity-typed object surface is resolved to a canonical
entity. Resolution runs read-only and is **confined to the episode's namespace** — there is no global
entity pool, which is a safety boundary, not an oversight. For each surface the pipeline tries, in
order:

1. **Intra-episode coreference** — the same or a token-subset surface already seen this episode.
2. **Exact name/alias match** over the BM25 entity index, case- and spacing-insensitive.
3. **Embedding clustering** — the nearest entity within `merge_threshold` (default cosine distance
   0.12) of the surface embedding.

Failing all three, the surface forms a **new entity** — the conservative default. A wrong merge
fuses two distinct things and is far harder to undo than a wrong split, so resolution leans toward
splitting. A new entity's id is a content hash over namespace, type, and the *normalized* name (the
same normalization the exact-match gate uses), so two surfaces differing only in case or whitespace
mint one id rather than splitting into duplicates, and the same surface always resolves the same way.

## Supersession and contradiction detection

`detect` is a pure function over the committed current facts (`current_support_facts`, scoped to the
`(subject, predicate)` pairs this episode touches and to the episode's namespace) and the newly
extracted ones. It produces only instructions; the store materializes the edges in the flip.

- A predicate is **multi-valued by default** — additive, nothing retired. This is the conservative
  choice: a wrong "functional" mark would silently retire facts that should accumulate.
- A **functional** predicate (e.g. `based_in`, `located_in`) holds exactly one current object per
  subject. A newer different object supersedes the prior one; the loser is retired into history with
  a closed window, never dropped. Which assertion wins is a pure function of the assertions
  themselves — the greater event time, ties broken by canonical object order — so the current value
  converges to the same answer regardless of the order episodes happened to consolidate in.
- **Mutually-exclusive** objects (the always-on boolean inversion rule, plus any configured antonym
  pairs) raise a `Contradiction`. The lower-trust side is the victim, deterministically and
  symmetrically — never keyed on which side happened to be the incumbent.

### Quarantine and the reconcile signal

A contradiction is *recorded* whether or not it matters much. It is **quarantined** only when the
pair carries real weight: either side at or above the `high_trust_threshold` (default 0.7). A
quarantine actively flags the victim for review and raises one `Quarantine` audit event — the
surfaced reconcile signal — naming the quarantined value, its trust, and the surviving value. The
victim's node and `CONTRADICTS` edge are retained; the recall path excludes the contradiction's
source by edge presence, so a quarantined value does not surface as current, but nothing is destroyed.

## Conservative summarization into notes

After detection, the pass rolls up each touched subject's facts — the just-extracted ones plus the
committed current support about that subject — into summary `Note`s. This is conservative on three
fronts:

- **It only fires when there is something worth condensing.** A cluster forms only if it clears the
  size gates: at least `min_facts` (default 3) facts about a subject and `min_entities` (default 2)
  distinct entities.
- **A detail-retention guard blocks lossy notes.** A produced summary must name a high fraction of
  the cluster's distinct entities (`entity_retention_threshold`, default 0.9, checked with
  whole-word matching so a substring can't inflate the count) *and* the cluster's mean source
  confidence must clear `confidence_floor` (default 0.6). A summary that fails either check is
  **skipped, not written** — the raw facts stay exactly as they were. Every cluster, written or
  skipped, emits a `summarize` audit event.
- **Notes carry lineage.** Each surviving note links back to its source facts and to the originating
  episode, and its id is content-addressed over the source fact set and the summarizer rule version,
  so a replay produces the same note (a no-op) and a grown fact set produces a *new* note while
  keeping the old one.

The shipped `RuleSummarizer` renders a deterministic templated body that names every predicate and
entity in the cluster, so it passes the guard by construction. The guard exists for the summarizers
that don't — a future model-backed one. The model-backed distiller is the optional, off-cursor layer
documented in [distillation](distillation.md); it never replaces these canonical notes.

## What it does not do

- It **never destroys a raw episode.** An episode is flipped to `consolidated` and kept; nothing in
  this pipeline deletes source memory. Superseded and contradicted facts are likewise retained, not
  removed.
- It does **not** re-enter the write path from inside a pass. Passes are read-only by contract.
- It does **not** run a model on the critical path. The shipped extractor and summarizer are
  deterministic rules; the optional LLM layers run off this cursor, and turning them on cannot move
  any of the reproducible state described here.
- It does **not** resolve entities across namespaces, and a supersession or contradiction edge can
  never bridge one. Resolution and detection are namespace-local by construction.

## Observability

Each tick reports what it accomplished — episodes consolidated, retried, failed, and the backlog
still pending — and emits lag gauges (`consolidation_lag_seconds`, `consolidation_episodes_pending`,
`consolidation_episodes_failed`) plus per-tick counters for supersessions, contradictions,
quarantines, and summaries. When the oldest pending episode is older than `lag_ceiling` (default 5s),
the scheduler warns. The lifecycle of every episode — and every refusal, failure, canonicalize
decision, and quarantine — lands in the audit trail, so the whole pipeline is reconstructable after
the fact.
