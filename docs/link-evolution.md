# Note link evolution

Link evolution is the deterministic, off-cursor layer that draws and revises relationships between
notes — `RELATES_TO` edges like `subsumes`, `contradicts`, or `elaborates`. It runs against
already-committed notes and is built so that running it cannot move the reproducible parts of the
system.

> **Off-cursor and off the critical path.** Nothing turns on unless you configure an evolver and
> enable it. Even then it runs *outside* the consolidation cursor, in its own transaction, so the
> canonical write-and-consolidate path is byte-identical whether link evolution is on, off, or
> unavailable. The edges it writes are non-canonical and never enter the current-fact recall path.

## The deterministic evolver behind a seam

The driver is generic over a `LinkEvolver` seam, and the shipped implementation is deterministic:

- **`RuleLinkEvolver`** ([`crates/aionforge-consolidate`](../crates/aionforge-consolidate)) draws the
  one relationship a pure-vector method can infer: `related_to`, to each candidate, with the
  source-to-candidate embedding cosine as the confidence. It is `Infallible` and reproducible, so the
  driver is testable with no network and link evolution is byte-deterministic for the same notes and
  rule version.

Because the implementation is named behind the `LinkEvolver` seam, a deployment can inject a
different evolver without touching the driver. Any such evolver that calls inference is gated by the
[cross-family consolidation guard](cross-family-guard.md) before each model call; the shipped rule
evolver declares no model family and is outside the guard's scope.

## Why off the cursor

A non-deterministic implementation behind the seam would make a consolidation-cursor commit
non-deterministic — different committed bytes on replay. Link evolution therefore never runs inside
the cursor, even though the shipped evolver is itself deterministic. Its edges are non-canonical:
they sit alongside canonical recall and never enter the current-fact path, so running it cannot
change deterministic recall either. It runs on its own schedule and cannot stall consolidation.

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

An evolver that misbehaves should not be able to churn the graph, so every run is bounded:

- the closed **relationship vocabulary** (`related_to`, `contradicts`, `subsumes`, `elaborates`,
  `depends_on`) — never evolver free text, an anti-injection and anti-drift constraint the driver
  enforces even if a future evolver does not;
- a **confidence floor** below which proposals are dropped;
- per-run caps on **links created**, **links revised**, and **distinct notes affected**;
- a **per-pair revision cap** counted from the pair's closed versions, so the same relationship
  cannot be rewritten without bound.

When an inference-backed evolver is injected behind the seam, the prompt is hardened the same way the
recall bundle's is: the source and candidate notes are rendered as untrusted third-party data inside
structural tags with the tag delimiters escaped, and the evolver is asked for a fixed
`LINK <candidate-number> <label> <confidence>` line grammar. Every line is parsed strictly — a forged
line, an out-of-vocabulary label, or a candidate number that was never offered is dropped. Injection
*steering* of an inference-backed evolver is only partially mitigated in 1.0, an honest-scope limit;
the shipped `RuleLinkEvolver` runs no model and is not exposed to it at all.

## Provenance

The edge schema carries no model field, so each run's provenance — the evolver's model family and
version (empty for the deterministic rule evolver), the endpoint and pinned seed when an
inference-backed evolver supplies them, and the per-pair decisions (created or revised, with the
label and confidence) — is recorded in a `link_evolve` audit event wired straight to the source note
(`AuditEvent -AUDIT-> Note`). Any API key never appears in the payload. This is what the
[cross-family consolidation guard](cross-family-guard.md) reads to verify which model family proposed
a relationship — since M6.T01 each source note is checked before any inference call, with the note's
own authoring model unioned into its writer set so an author-then-evolve launder cannot pass.

## Running it

The evolver is injected, not built into the facade, so you choose (and gate) the implementation:

```rust
use aionforge_consolidate::{RuleLinkEvolver, LinkEvolveConfig};

let evolver = RuleLinkEvolver::with_default_rules();
let config = LinkEvolveConfig {
    enabled: true,
    seed: Some(42), // recorded in provenance
    ..LinkEvolveConfig::default()
};
let report = memory.evolve_links(evolver, &namespace, config, &now).await?;
```

Link evolution needs no embedder at call time — it scores the notes' already-stored embeddings. Call
it when it suits the deployment — at session end, on a timer, or from a tool. With `enabled` unset it
is a no-op. `now` is supplied by the caller; the facade keeps no ambient clock, so a link's
transaction time is deterministic.

## Status

Link evolution ships **off by default**. It is a tested, self-contained capability with no caller on
the core path; auto-promotion of a discovered link into the canonical tier is deferred to the trust
milestone. The `LinkEvolver` seam is open for an inference-backed evolver, which the
[cross-family consolidation guard](cross-family-guard.md) gates before each model call.
