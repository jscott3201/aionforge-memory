# LLM distillation

Distillation is the optional, off-by-default layer that condenses a subject's facts into a
higher-level note with a chat model, instead of the deterministic rule summarizer. It is the one
place a generative model touches stored content — so it is built to never get near the parts that
have to stay reproducible.

> **Optional, off by default, and off the critical path.** Nothing turns on unless you configure a
> completer and enable distillation. Even then it runs *outside* the consolidation cursor, so the
> canonical, byte-deterministic write-and-consolidate path is identical whether distillation is on,
> off, or unavailable. It is experimental in 1.0 and stays off until it clears the distillation
> benchmark.

## Two tiers, side by side

There are two summary tiers and they do not interfere:

- **Canonical (rule).** The deterministic [rule summarizer](../crates/aionforge-consolidate) runs
  inside the consolidation cursor's atomic flip, exactly as before. Its notes are byte-identical on
  replay.
- **Distilled (LLM).** The distiller runs separately, reads the already-committed facts, and writes
  its notes in its own transaction. Its notes are non-canonical.

The two never collide because a distilled note's id is content-addressed under the distiller's own
rule version (`llm-distill-v1`), a different id-space from the rule summaries (`summarize-v1`). So
both can describe the same subject and both land. "Degrade to the canonical tier" is therefore
structural, not a fallback you have to code: when the model is unavailable, the rule notes are
simply what remains.

## Why off the cursor

Putting a non-deterministic model call inside the cursor's commit would make the consolidation path
non-deterministic — a different generation on replay, different committed bytes. The whole point of
the layered design is that enabling distillation *cannot perturb* the canonical path. So the model
never runs there. Distilled notes are non-canonical `DERIVED_FROM`-linked nodes that sit alongside
canonical recall and never enter the current-fact path, so they cannot change deterministic recall
either.

A second benefit falls out of this: there is no cursor latency to manage. The distiller is bounded
only by the completer's own timeout, not by the scheduler's per-episode budget, so a slow model
cannot stall consolidation — it just produces fewer notes this run.

## What a run does

`Memory::distill` runs the distiller over one namespace's current support facts:

1. read the namespace's current support facts and group them into per-subject clusters (the same
   conservative size gates the rule summarizer uses);
2. ask the model to condense each cluster, from an instruction-free, structurally-tagged prompt;
3. check each summary against the **detail-retention guard** — the same guard the rule path uses —
   and drop any that lose too much specificity;
4. embed the survivors and write them as distilled notes with `DERIVED_FROM` lineage to their
   source facts, plus a `distill` provenance audit for every call.

It returns a small report — clusters seen, notes written, summaries rejected as lossy, calls
declined — while the per-call detail lives in the audit events.

## The guard is the safety net

A chat model can hallucinate, drop an entity, or get truncated. The detail-retention guard is what
makes that safe: a summary is only written if it preserves enough of the cluster's distinct
entities (whole-word, so `Bo` is not credited inside `Bobby`) and the cluster clears a confidence
floor. A summary that drops too much is rejected and never stored — the raw facts and the canonical
rule note remain. A completion truncated at the token cap (`finish_reason == "length"`) is treated
as lossy and rejected before the guard even sees it.

The prompt itself is hardened: the cluster is rendered as untrusted third-party data inside
structural tags, with the tag delimiters escaped in the content so a crafted fact cannot forge a
tag or impersonate an instruction. The system frame is a fixed, minimal template. Injection
*steering* of the distiller is only partially mitigated in 1.0 — a limit stated plainly in the
honest-scope notes; the cross-family guard addresses trait transfer, not steering.

## Provenance

The note schema carries no model field, so each call's provenance — the model family and version,
the endpoint, the pinned seed, and the outcome (written, rejected as lossy, or declined) — is
recorded in a `distill` audit event. For a written note the audit is wired straight to the note
(`AuditEvent -AUDIT-> Note`), so "which model produced this note" is one hop, and unambiguous even
when a note rolls up facts about several entities. The API key never appears in the payload. This
is what the cross-family consolidation guard reads to verify the consolidating model family
differs from the writer's.

All of it is queryable in one call: `Memory::note_lineage(&note_id)` returns the note's source
facts and episodes (the `DERIVED_FROM` walk), the model that authored it (decoded from the
`distill` audit; `None` for a deterministic rule summary), the writer families behind its sources
(signed provenance record first, the episode's origin copy when no record exists, the agent's
current declaration last — an unresolvable or empty family reads back as `unverifiable`, never
silently dropped), and an explicit `non_canonical` marker. It is a point read: the producing model
lives in audit payload, not an indexed column, so filter-by-model scans should drive the guard
surface instead.

## Running it

The distiller is injected, not built into the facade, so the engine stays off the chat-client crate
and you choose (and gate) the model:

```rust
use aionforge_consolidate::{DistillationConfig, LLMSummarizer};

// `completer` is an aionforge-chat HttpCompleter built from your CompleterConfig.
let summarizer = LLMSummarizer::new(completer).with_max_tokens(4096);
let config = DistillationConfig {
    enabled: true,
    endpoint: Some(endpoint.clone()), // recorded in provenance (the base URL, not a secret)
    seed: Some(42),                   // recorded in provenance
    ..DistillationConfig::default()
};
let report = memory.distill(summarizer, &namespace, config, &now).await?;
```

Call it when it suits the deployment — at session end, on a timer, or from a tool. With
`enabled` unset it is a no-op. `now` is supplied by the caller; the facade keeps no ambient clock,
so a distilled note's transaction time is deterministic.

## Status

Distillation ships **off and experimental**. It graduates from experimental only by clearing the
distillation-quality benchmark against the rule-summarizer baseline; until then it is a tested,
self-contained capability with no caller on the core path. See the
[completion client](completion-client.md) for the model seam it runs on.
