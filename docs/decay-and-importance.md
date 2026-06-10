# Decay and importance scoring

How a memory's relevance ages (05 §2, M5.T01). A memory is written with an importance
score; that score is the anchor, not the living value. At read time the substrate
computes an **effective importance** — the stored score sunk by elapsed time under a
per-tier exponential half-life — and ranks with it. Relevance in recall is
three-factor: what the query matches (the lexical/dense/graph search signals), how
important the memory is now (the importance re-rank), and how recently it entered the
record (the recency re-rank).

## The decay model

Effective importance is a pure function of four inputs: the stored importance, the
memory's `last_access` instant, the caller-supplied `now`, and the tier's half-life.

```text
effective = stored × 0.5 ^ (elapsed_seconds(last_access → now) / half_life_seconds)
```

One half-life halves the score, two quarter it, and the curve never reaches zero —
decay alone never erases anything. Elapsed time is measured in whole seconds (ample for
half-lives measured in days) from the UTC instant difference, so the result does not
depend on either side's time-zone representation.

**Two tiers, not three** (05 §2). Session-scoped episodic memory decays on the short
half-life; everything semantic decays on the long one:

| Tier | Kinds | Default half-life |
| --- | --- | --- |
| Episodic | `Episode` | 7 days |
| Semantic | `Fact`, `Entity`, `Note`, `Skill`, `BadPattern`, `CoreBlock` | 365 days |

Identity memory (`CoreBlock`) deliberately folds into the semantic tier rather than
carrying a third knob: the spec names a short class and a long class, and a third
half-life would be configuration with no behavior behind it. Kinds without a `Stats`
block — forensic, control, and agent records — have nothing to decay and no tier. The
mapping is keyed on the kinds' own label constants, so it moves with a rename instead
of drifting from it.

**Derived, never stored** (§13.7). The substrate keeps no authoritative copy of derived
state, so the decayed value is never written back: retrieval computes it at rank time,
and the active-forgetting sweep (M5.T02) recomputes it at sweep time, through the same
pure functions. Repeating a recall at the same instant is byte-identical — a clocked
recall stays exactly as read-only as an unclocked one. Decay is therefore not audited
either: it is rank-time arithmetic, not a lifecycle action. The audited action is the
forgetting sweep that consumes it (M5.T02).

**Defensive short-circuits.** Four inputs return the stored value unchanged rather than
minting a surprise: a pinned memory (below); an inert half-life (non-finite or
non-positive means "no decay for this tier" and keeps the division well-defined); a
non-finite stored importance (garbage in is the same garbage out, never a minted NaN —
NaN fails every comparison and would otherwise read as ineligible downstream); and a
non-positive elapsed time (a future-stamped `last_access` clamps to zero elapsed, so a
clock regression can never *inflate* importance).

## Pinning

A pinned memory never decays out of eligibility (05 §2). The pin short-circuits inside
the decay function itself — a pinned memory keeps its full write-time importance in
every ranking — and the shared eligibility rule holds a pinned memory eligible against
any floor. The pin is a plain branch on the stored `Stats` scalar, never routed through
a loss-tolerant recompute. The retrieval re-rank only *orders* by importance and never
drops a candidate, so eligibility is consumed by the M5.T02 soft-expire sweep, not by
recall.

## The forget-eligibility seam

The sweep side of that seam is the pure `forget_eligible` predicate (05 §2, M5.T02): a
strict AND across the pure axes — unpinned, decayed importance below the sweep's floor,
trust below the trust floor, unreferenced, and at least a minimum age — where any single
axis can only *spare* a candidate, never doom one on its own. The pin is double-enforced:
the predicate checks it explicitly, and the shared eligibility rule absorbs it through
the pin override, so no misconfigured floor can forget a pin. Non-finite importance or
trust scalars spare rather than doom — the sweep never destroys on a value the
arithmetic cannot vouch for. The graph-side axes (kind scoping, attestation, promotion
lineage, and the reference probe itself) belong to the forgetting orchestrator, not to
these pure functions; soft-forget is also the only revision channel that writes a bare
`expired_at` with the status untouched, which is what keeps it distinguishable from
supersession, contradiction, and reliability demotion at read time.

On the store side, the sweep reads through two surfaces. A candidate page enumerates the
sweep-scoped kinds (`Episode` and `Fact` only — identity memory is hard-exempt, skills
belong to deprecate-never-delete, bad patterns are protected negative knowledge) with
already-expired nodes filtered at the source, keyset-paginated over `(label, id)` so a
resumed sweep visits exactly what one full scan would. The "unreferenced" axis is
answered by a live-edge probe over a node's incoming references — the authoritative
signal is the edges themselves, never the loss-tolerant `referenced_count` cache, and a
closed `RELATES_TO` version no longer protects.

The writes are a single flip. Soft-forget sets `expired_at` and touches nothing else —
status stays `Active`, no edge is written or closed — and un-forget removes the key,
restoring the byte-identical record. Each co-commits its audit row in the same
transaction, gated on a real state transition, so a replay converges with no second
event. The orchestrator that builds those events addresses them to the memory's own
namespace, where its agent can see them — the store funnel commits whatever identity
the caller minted.
Both directions refuse a node whose status another channel owns: soft-forgetting a
quarantined or superseded record would manufacture an ambiguous lifecycle signature,
and un-forgetting a demotion's expiry would resurrect what governance retired —
re-promotion owns that path. Reversibility holds until the retention prune physically
removes the record.

At read time the exclusion is one node-level gate per path. Episodes and skills already
dropped a node-level `expired_at` from their default reads; facts gain the single new
check in the per-candidate resolver, after the namespace gate and before the temporal
predicate — deliberately not in the support provider (labels and edges only) and not in
the temporal predicate (status and the ABOUT window), so soft-forget stays orthogonal to
supersession and one mechanism owns the exclusion. `include_expired` is the one
retention flag, honored in every temporal mode: a history read sets it to see forgotten
records, and as-known-at semantics are untouched because the forget never moves an edge
window.

The orchestrator ties the layers together, cheapest protection first: pinned, then the
promotion-lineage and attestation probes (governance territory and the M5.T03 boundary),
then the pure axes through the domain predicate. A point-forget runs the same full gate
as the sweep — a host cannot force-forget a protected memory, and instead of a silent
no-op it learns which protection held (pinned, attested, lineage, referenced, too young,
the kind itself, or an axis still above its floor). Un-forget takes no eligibility gate
on the way back: restoring a memory is always safe, only the demotion shape stays
refused. Every applied forget writes its decision basis into the audit payload — kind,
tier, the decayed importance and trust against their floors — so the reversible window
is explainable after the fact.

## The caller's clock

There is no ambient clock in the retrieval path (03 §6). The importance and recency
re-ranks run only when the caller stamps an instant onto the query's options
(`RecallOptions::now`); the default `None` is the honest "no clock was provided" state
and leaves the recall byte-identical to a pre-decay one. The MCP server is the
canonical clocked caller: its `search` handler stamps the host's wall clock onto every
recall, exactly as `capture` stamps `captured_at` — the host boundary owns the clock,
the substrate never reads one. Supplying the clock is necessary, not sufficient: each
query class still decides whether it weights these re-ranks (the quote class keeps both
off; see [Retrieval](retrieval.md)).

## Ranking integration

Importance and recency are **re-ranks**: they order only the candidates the search
signals already surfaced, per kind, and can never widen a recall (03 §2). Each builds a
competition ranking — equal scores share a rank, so a uniform field contributes a
constant to every candidate and reorders nothing — and feeds reciprocal-rank fusion
under the query class's weight, exactly like the trust re-rank.

The two signals read different time axes on purpose. Importance decays from
`last_access` — how stale the memory's use is. Recency orders by the ingestion instant —
how new the record is. A memory can be old but recently touched, or fresh but already
idle; the two re-ranks rank those differently, and neither double-counts the other.

With decay disabled, the importance re-rank still participates and orders by the raw
stored importance; the switch governs only whether elapsed time sinks the value.

## Configuration

Decay is off by default. The host's configuration carries a `decay` section —
`enabled`, `episodic_half_life_secs`, `semantic_half_life_secs` (validated non-zero
when enabled) — and maps it into the retriever's configuration, the half-lives carried
as `f64` seconds: the same host-side indirection as the reliability policy, so neither
the engine nor the retrieval crate takes a config dependency. The conservative defaults
(seven days episodic, a year semantic) reflect the spec's posture that aggressive
forgetting risks losing rarely-but-critically-needed memories; deployments tune from
there.

On the engine, forgetting is three facade methods behind one off-switch: `forget` and
`unforget` by id (point ops, host-cadence, caller-supplied clock) and `sweep_forgetting`
(one candidate page per call; the report's `next` is the watermark to persist and pass
back, and a resumed walk visits exactly what one uninterrupted scan would). With the
policy disabled the engine builds no forgetter at all — the sweep returns an empty
report without reading the graph, and the point ops answer `Disabled` rather than
fabricating a "not found". The sweep enumerates the all-namespaces spine like the
reliability sweep (forgetting is substrate maintenance, not a principal-scoped read),
while each audit row stays agent-visible in the forgotten memory's own namespace.

The forgetting sweep (M5.T02) has its own `forgetting` section — also off by default —
carrying the floors the sweep measures candidates against: `importance_floor`,
`trust_floor`, `min_age_secs`, the per-page `batch_cap`, and a `forget_bad_patterns`
toggle that keeps negative knowledge protected unless a deployment opts it in. The
section deliberately re-declares no half-lives: the sweep's decayed-importance axis
reads the `decay` section's, so rank-time and sweep-time aging can never disagree.
Validation when enabled keeps each floor finite in `[0.0, 1.0]` and rejects both floors
at the ceiling together — a configuration that would make nearly every unpinned memory
a sweep candidate. The section ships ahead of the sweep that consumes it, the same way
the decay section landed before its retrieval wiring.
