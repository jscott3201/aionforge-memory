# Note link evolution

Link evolution is the optional, off-by-default layer that draws and revises relationships between
notes — `RELATES_TO` edges like `subsumes`, `contradicts`, or `elaborates`. It is the second place
a chat model may touch stored memory (the first is [distillation](distillation.md)), and like the
distiller it is built so that turning it on cannot move the reproducible parts of the system.

> **Optional, off by default, and off the critical path.** Nothing turns on unless you configure an
> evolver and enable it. Even then it runs *outside* the consolidation cursor, in its own
> transaction, so the canonical write-and-consolidate path is byte-identical whether link evolution
> is on, off, or unavailable. The edges it writes are non-canonical and never enter the current-fact
> recall path.

## Two tiers, side by side

There are two evolvers behind one seam, and they do not interfere:

- **Rule (deterministic).** [`RuleLinkEvolver`](../crates/aionforge-consolidate) draws the one
  relationship a pure-vector method can infer: `related_to`, to each candidate, with the
  source-to-candidate embedding cosine as the confidence. It is `Infallible` and reproducible, so
  the driver is testable with no network and the rule tier is always available beneath the model.
- **LLM.** [`LLMLinkEvolver`](../crates/aionforge-consolidate) asks a chat model which of the
  closed-vocabulary relationships hold and how strongly. When the model is unavailable, truncated,
  or unusable, the call is declined and the run degrades to the rule tier — no edge is forced.

Both implement the same `LinkEvolver` seam, so the off-cursor driver does not know or care which one
it is running.

## Why off the cursor

A non-deterministic model call inside the cursor's commit would make the consolidation path
non-deterministic — a different generation on replay, different committed bytes. Link evolution
therefore never runs there. Its edges are non-canonical: they sit alongside canonical recall and
never enter the current-fact path, so enabling it cannot change deterministic recall either. A slow
model cannot stall consolidation, because the evolver runs on its own schedule, bounded only by the
completer's timeout.

## One current relationship per pair, kept honestly

`RELATES_TO` is **bi-temporal**, exactly like a `Fact`'s validity window: each edge carries
`valid_from`/`valid_to` and `ingested_at`/`expired_at`, and an edge is *current* while its
`valid_to` is unset. The store enforces **one current relationship per ordered `(source, target)`
pair**:

- proposing the same label that is already current is an idempotent no-op;
- proposing a *different* label is a **revision** — the prior version is closed (its `valid_to` is
  set) and a new version is opened, the same close-and-replace shape as fact supersession.

So a relationship's history is never lost — a relabeling leaves the old version closed, not deleted —
and a stale or flip-flopping proposal can never fork a pair into two simultaneously-current links.

## What a run does

`Memory::evolve_links` runs the evolver over one namespace's live notes, off the cursor:

1. pool the namespace's live notes that carry an embedding (a candidate needs a vector), id-sorted
   and bounded, so the run is reproducible;
2. for each source note, offer the evolver its nearest embedding neighbors as candidates;
3. ask the evolver which relationships hold, from an instruction-free, structurally-tagged prompt;
4. validate every proposal — the label must be in the closed vocabulary, the confidence must clear
   the floor, and the target must be one of the candidates that was offered;
5. decide per pair whether to create, leave alone, or revise, and write the survivors through the
   store's bi-temporal link surface in one transaction.

It returns a small report — source notes seen, links created, links revised, calls declined — while
the per-call detail lives in the `link_evolve` audit events.

## The cascade guard

A model that misbehaves should not be able to churn the graph, so every run is bounded:

- the closed **relationship vocabulary** (`related_to`, `contradicts`, `subsumes`, `elaborates`,
  `depends_on`) — never model free text, an anti-injection and anti-drift constraint the driver
  enforces even if a future evolver does not;
- a **confidence floor** below which proposals are dropped;
- per-run caps on **links created**, **links revised**, and **distinct notes affected**;
- a **per-pair revision cap** counted from the pair's closed versions, so the same relationship
  cannot be rewritten without bound.

The prompt itself is hardened the same way the distiller's is: the source and candidate notes are
rendered as untrusted third-party data inside structural tags, with the tag delimiters escaped (the
escape is shared with the distiller so the defense is defined in one place), and the model is asked
for a fixed `LINK <candidate-number> <label> <confidence>` line grammar. Every line is parsed
strictly — a forged line, an out-of-vocabulary label, or a candidate number that was never offered
is dropped. Injection *steering* is only partially mitigated in 1.0, the same honest-scope limit the
distiller carries.

## Provenance

The edge schema carries no model field, so each call's provenance — the model family and version,
the endpoint, the pinned seed, and the per-pair decisions (created or revised, with the label and
confidence) — is recorded in a `link_evolve` audit event wired straight to the source note
(`AuditEvent -AUDIT-> Note`). The API key never appears in the payload. This is what a later
cross-family consolidation guard reads to verify which model family proposed a relationship.

## Running it

The evolver is injected, not built into the facade, so the engine stays off the chat-client crate
and you choose (and gate) the model:

```rust
use aionforge_consolidate::{LLMLinkEvolver, LinkEvolveConfig};

// `completer` is an aionforge-chat HttpCompleter built from your CompleterConfig.
let evolver = LLMLinkEvolver::new(completer).with_max_tokens(1024);
let config = LinkEvolveConfig {
    enabled: true,
    endpoint: Some(endpoint.clone()), // recorded in provenance (the base URL, not a secret)
    seed: Some(42),                   // recorded in provenance
    ..LinkEvolveConfig::default()
};
let report = memory.evolve_links(evolver, &namespace, config, &now).await?;
```

Unlike the distiller, link evolution needs no embedder at call time — it scores the notes'
already-stored embeddings. Call it when it suits the deployment — at session end, on a timer, or from
a tool. With `enabled` unset it is a no-op. `now` is supplied by the caller; the facade keeps no
ambient clock, so a link's transaction time is deterministic.

## Status

Link evolution ships **off and experimental**. It is a tested, self-contained capability with no
caller on the core path; auto-promotion of a discovered link into the canonical tier is deferred to
the trust milestone. See the [completion client](completion-client.md) for the model seam it runs on
and [distillation](distillation.md) for its sibling layer.
