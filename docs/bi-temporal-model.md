# The bi-temporal model

Every fact in Aionforge Memory carries two independent clocks. One records when the
thing was true in the world; the other records when the substrate came to believe it.
Keeping the two apart is what lets a recall answer "what is true now," "what was true
last March," and "what did we think we knew on the day we acted" without any of those
questions stepping on the others.

## Two clocks, four timestamps

The two windows live in one block, [`BiTemporal`](../crates/aionforge-domain/src/time.rs),
carried on a fact's `ABOUT` edge (and on the supersession and contradiction edges):

```rust
pub struct BiTemporal {
    pub valid_from:  Timestamp,          // event-time lower bound
    pub valid_to:    Option<Timestamp>,  // event-time upper bound; None while current
    pub ingested_at: Timestamp,          // transaction-time lower bound (immutable)
    pub expired_at:  Option<Timestamp>,  // transaction-time upper bound; None while live
}
```

- **Event time** (`valid_from`/`valid_to`) is when the underlying fact became and stopped
  being true in the world. "Acme was based in NYC from 2021 until they moved in 2026."
- **Transaction time** (`ingested_at`/`expired_at`) is when the substrate recorded and
  later retired its belief. "We learned about the NYC address on the day we ingested the
  email, and we expired that belief the day the move was confirmed."

An open (`None`) upper bound means "still in effect." The two are orthogonal: a fact can be
valid in the world over one span and believed by the substrate over a completely different
one, which is exactly the case the model exists to represent. A late-arriving correction has
an old event window and a new transaction window; a retracted belief has a closed
transaction window over a fact that was never wrong about the world.

The window lives on the **edge, not the node**. The `Fact` node holds the triple
(`subject_id`, `predicate`, `object`), its confidence, and a status; its validity is the
four timestamps on the `Fact -ABOUT-> Entity` edge. That placement is deliberate. Currentness
in this substrate is modeled by **edge presence**, not by a flag on the node, so the validity
that drives currentness belongs with the edges that carry it.

One invariant guards every window the write path produces: `windows_ordered` requires neither
lower bound to sit after its present upper bound. The write path fails closed rather than
persist an out-of-order window, so a supersession can never close a window before the fact
even began.

## The four retrieval modes

A recall chooses which slice of history it reads facts against through `TemporalMode` on
[`RecallOptions`](../crates/aionforge-retrieval/src/query.rs). The supplied instant is always
caller-provided; there is no ambient clock in the retrieval path, which is what keeps recall
deterministic.

```rust
pub enum TemporalMode {
    Current,                // what is true now (the default)
    AsOf(Timestamp),        // event time: what was true in the world at t
    AsKnownAt(Timestamp),   // transaction time: what we believed at t
    History,                // the whole record, every status and window
}
```

The window test itself is a small pure function,
[`fact_passes_temporal`](../crates/aionforge-retrieval/src/temporal.rs), kept apart from the
retriever's I/O so it can be reasoned about on its own:

- **`Current`** keeps facts whose `status == active`. The structural work is already done
  upstream: the candidate set is scoped to the `current_support_facts` provider, which has
  already removed anything superseded or contradicted by edge presence. This mode applies
  only the scalar half the provider rule cannot express.
- **`AsOf(t)`** reads the **event** window, half-open: a fact passes iff
  `valid_from <= t < valid_to`, with an open `valid_to` unbounded above. Status is irrelevant
  here. A fact that has since been superseded still answers "yes, that was true then."
- **`AsKnownAt(t)`** reads the **transaction** window, half-open: a fact passes iff
  `ingested_at <= t < expired_at`, with an open `expired_at` unbounded above. This is the
  audit question — what the substrate believed at an instant — independent of whether the
  belief was correct about the world.
- **`History`** keeps every status and every window, including superseded, contradicted, and
  quarantined facts. It is the explicit opt-in for an audit or history view, never the default.

Both windowed modes are exclusive on the upper bound. A query at exactly `valid_to` (or
`expired_at`) does not match the closing fact; that instant belongs to its successor. Doing
the comparison in Rust rather than in the query is itself a choice: selene-db cannot index a
zoned datetime, so the candidate set is bounded by search first and filtered by instant here.

These modes shape **facts** only. Episodes are raw turns with no validity window, so they
surface in every mode and are gated separately by `include_expired`, which controls
soft-forgotten episodes and nothing else.

## Supersession and contradiction are non-destructive

When a newer assertion replaces an older one, or two assertions disagree, the substrate never
deletes. Both operations are recorded as edges and leave the prior facts in place.

**Supersession** (`SUPERSEDED_BY`, a `Fact -> Fact` edge). Applying one closes the superseded
fact's `ABOUT` event-time window (`valid_to <- valid_from` of the supersession), writes
`old -SUPERSEDED_BY-> new`, and mirrors `old.status = superseded`. The prior fact and all its
data are kept. The direction follows the detector's convergence order, not arrival order: the
common case retires a committed incumbent with a newer assertion, but a stale assertion that
arrives after a newer incumbent is itself born superseded, so `old` is then the just-created
fact. Application is idempotent. If the window is already closed at exactly this instant the
call is a no-op, so replaying an episode after a crash re-applies nothing.

**Contradiction** (`CONTRADICTS`, a `Fact -> Fact` edge). Applying one writes
`source -CONTRADICTS-> target` and, when the source is quarantined (a new fact contradicting a
high-trust current one), mirrors `source.status = quarantined`. Both facts are retained. There
is no read-guard here because writing the edge once-only and re-setting a status to the same
value are both already idempotent.

The reason these are non-lossy is the whole point of the model. A destructive update would
make `AsOf` and `AsKnownAt` impossible — you cannot ask what was true last March if the record
of last March was overwritten. `status` on the node is a **redundant scalar mirror** of the
edge-presence state, a fast filter for the `Current` path, not the source of truth. The source
of truth is the edges.

## The maintained current-state providers

Computing "what is true now" by walking supersession and contradiction edges on every query
would be slow, so the substrate keeps five maintained candidate-state sets the engine updates
incrementally as the graph mutates. Each is a membership rule over labels and edge
presence/absence, wired in
[`providers.rs`](../crates/aionforge-store/src/providers.rs) and referenced by stable name
through the `CandidateSet` enum:

- **`current_support_facts`** — a `Fact` with no live outgoing `SUPERSEDED_BY` and no live
  outgoing `CONTRADICTS`. Both edges remove their *source* (the superseded fact, the
  quarantined contradicting fact), so both are excluded outgoing. This is the structural
  current set; the `status = 'active'` half is the query-time scalar filter `Current` mode
  layers on top.
- **`provenance_current_support_facts`** — the same, plus a required incoming `SUPPORTS` and a
  required outgoing `HAS_PROVENANCE`. A sensitive query reads against this set instead, so an
  ungrounded fact never surfaces.
- **`scope_membership`** — anything with a live outgoing `IN_SCOPE` edge. The coarse "in some
  scope" set; per-scope selection is query-time set algebra over it.
- **`recency_active`** — anything with a live outgoing `RECENT_IN` edge. Coarse, like scope.
- **`unresolved_current`** — a `Fact` that nothing currently contradicts: no live *incoming*
  `CONTRADICTS`. This is the deliberate dual of `current_support_facts`, which drops the
  contradiction *source* (outgoing). Keeping the directions opposite is what makes the set
  algebra pay off: `current_support_facts` minus `unresolved_current` is exactly the facts
  something contradicts but that are otherwise still current (the contested incumbents), while
  the intersection is the clean active set.

A provider rule can only test labels and edges; it cannot express a scalar predicate. That is
why `current_support_facts`' `status = 'active'` filter is applied at query time over the
provider's superset rather than encoded in the spec.

### Generation checking and the watermark

Reading a maintained set is generation-checked end to end. `candidate_state_infos` and
`candidate_state_members` return a result only when the provider has applied every commit
through the current graph generation; the engine binds the set to the same immutable snapshot
whose generation it validates the provider against. A stale provider surfaces as an **error**,
never an out-of-date set. The `generation` on a returned `CandidateStateInfo` is a live
watermark, not a hint. A successful call is itself the proof that no set is stale, which is the
property that lets the high-precision retrieval path trust the membership it composes against.

### Rebuilt from primary values, never persisted

The providers are not WAL or schema objects. They are `Arc<dyn IndexProvider>` attached to the
graph at construction and **re-attached on recovery**, so their specs are a code-level constant
the store builder wires in on every boot, not a migration statement. After a restart the engine
reconstructs each set's membership by replaying the primary node and edge values; the structural
current state is always re-derivable from the facts and their supersession/contradiction edges.

There is **no parallel index persisted on disk**. This is the honest scope of the design: the
providers are an in-memory acceleration over the primary graph, recomputed deterministically
from it, and the facts and edges are the durable record. Nothing the providers hold is a second
source of truth that could drift from the graph or have to be reconciled against it after a
crash.

## What it does not do

The retrieval modes filter facts; they do not rewrite history. `AsOf` and `AsKnownAt` read the
windows that are recorded and never interpolate a value the substrate never held. There is no
ambient clock anywhere in the retrieval path, so a windowed query is only ever as precise as the
instant the caller hands it. And the bi-temporal modes shape facts alone — episodes have no
validity window and are governed by soft-forgetting (`include_expired`), not by event or
transaction time. For how facts are written and deduplicated in the first place, see the
[consolidation](consolidation.md) path; for how a recall is gated by who is asking, see
[namespace authorization](namespace-authorization.md).
