# Active forgetting

How the substrate lets go (05 §2, M5.T02). Forgetting is a **soft expiry**: one
node-level `expired_at`, set with the status and every edge untouched, audited, and
reversible until the retention prune physically removes the record. It is off by
default, conservative by construction — every check can only spare a memory, never doom
one on its own — and strictly a default-recall notion: a forgotten memory leaves every
default read but stays in the record for history and audit.

## One signature among four

Four belief-revision channels each leave a distinct `(expired_at, status, edge)`
fingerprint, and that orthogonality is what makes un-forget safe:

| channel | node `expired_at` | node `status` | edge writes |
| --- | --- | --- | --- |
| soft-forget | set | untouched (stays `Active`) | none |
| supersession | untouched | `Superseded` | `ABOUT` window closed |
| contradiction | untouched | `Quarantined` | `CONTRADICTS` linked |
| reliability demotion | set | `Quarantined`, paired | lineage edge |

Soft-forget is the only channel that writes `expired_at` while leaving the status
untouched. Un-forget is therefore a bare key removal with no risk of resurrecting
something a *different* channel retired — and both store writes refuse a node whose
status another channel owns, so the table can never grow an ambiguous fifth row. The
audit kind (`Forget` / `Unforget`) is the authoritative discriminator for history
queries; the node state alone cannot tell a forget from a demotion, the kind always can.

## Eligibility: every axis can only spare

A memory is forgettable only when **all** of it holds — unpinned, decayed importance
below the floor, trust below the trust floor, unreferenced, and past the minimum age —
and no exemption fires first. The pure axes run through the domain's `forget_eligible`
predicate (the same decayed importance the retriever ranks with, through the same tier
half-lives, so rank-time and sweep-time aging can never disagree). The pin is
double-enforced: checked explicitly and absorbed by the shared eligibility rule, so no
misconfigured floor can forget a pin. Non-finite importance or trust scalars spare
rather than doom — the sweep never destroys on a value the arithmetic cannot vouch for.

Three graph-side exemptions run cheapest-first in the orchestrator before the axes:

- **Kind.** The sweep enumerates `Episode` and `Fact` only. Identity memory
  (`CoreBlock`) is hard-exempt, skills belong to deprecate-never-delete (point-forget
  works through the same expiry their retrieval already honors; the sweep stays out),
  bad patterns are protected negative knowledge behind a default-off toggle, and
  entities and notes are deferred.
- **Promotion lineage.** A node with a `PROMOTED_TO`/`DEMOTED_FROM` edge in either
  direction belongs to governance — a re-promotion would silently clear a soft-forget
  and a demotion would overwrite one, so lineage nodes are excluded outright.
- **Attestation.** An attested memory is refused entirely until the M5.T03 erasure
  cascade owns that edge.

The fourth graph probe is not an exemption but the "unreferenced" axis itself, one
conjunct of the strict AND: it reads live incoming edges from a protecting allowlist
(`DERIVED_FROM`, `SUPPORTS`, `DEPENDS_ON`, `RELATES_TO`, `HAS_FAILURE`, `MENTIONS`) —
never the loss-tolerant `referenced_count` cache. A closed `RELATES_TO` version no
longer protects; audit, provenance, scope, and session wiring never did (every memory
has those, and an allowlist that matches everything forgets nothing).

## The sweep and the point ops

The engine facade is three methods behind one off-switch. `sweep_forgetting` walks one
candidate page per call — keyset-paginated over `(label, id)`, already-expired nodes
filtered at the source — evaluates every candidate, soft-forgets the eligible, and
returns a tally whose `next` cursor is the watermark to persist; a resumed walk visits
exactly what one uninterrupted scan would. Like every maintenance driver it is
off-cursor and host-cadence with a caller-supplied clock, and it enumerates the
all-namespaces spine (substrate maintenance, not a principal-scoped read).

`forget(id)` runs the **same full gate** as the sweep: a host cannot force-forget a
pinned, attested, lineage, or protected-kind memory, and instead of a silent no-op it
learns which protection held. `unforget(id)` takes no eligibility gate on the way back —
restoring a memory is always safe — but a demotion's expiry stays refused. With the
policy disabled the engine builds no forgetter at all: the sweep returns an empty report
without reading the graph, and the point ops answer `Disabled` rather than fabricating a
"not found".

## The audit trail

Every applied transition co-commits its audit row in the same transaction as the
property flip, gated on a real state change — a crash-replay converges with no second
row. Events are cycle-addressed: `forget → unforget → forget` is three real decisions,
three rows. A forget row carries the decision basis in its payload — the reason
(`active_forgetting_sweep` or `manual`), the kind and tier, and the decayed importance
and trust against their floors — so the reversible window is explainable after the
fact. An unforget row carries its reason (`manual_unforget`) and the kind, nothing
more: restoring is always safe, so there is no decision basis to explain.
Rows land in the forgotten memory's **own namespace** — agent-visible through the scoped
audit reads, never hidden in substrate governance forensics.

## Retrieval exclusion

One node-level gate per path. Episodes and skills already dropped a node `expired_at`
from their default reads; facts gained the single new check in the per-candidate
resolver — after the namespace gate, before the temporal predicate, deliberately not in
the support provider (labels and edges only) and not in the temporal predicate (status
and the `ABOUT` window), so soft-forget stays orthogonal to supersession and one
mechanism owns the exclusion. `include_expired` is the one retention flag, honored in
every temporal mode; as-known-at semantics are untouched because a forget never moves an
edge window.

## Configuration

Off by default everywhere. The host's `forgetting` section carries the floors
(`importance_floor`, `trust_floor`), `min_age_secs`, the sweep's `batch_cap`, and the
`forget_bad_patterns` toggle; validation keeps each floor finite in `[0.0, 1.0]`,
rejects a zero batch cap, and refuses both floors at the ceiling together (mass deletion
misspelled as configuration). The half-lives feeding the importance axis are **not**
re-declared — they come from the `decay` section (see
[Decay and importance scoring](decay-and-importance.md)) and arrive regardless of
whether rank-time decay is enabled. The host maps the section plus the half-lives into
the engine's forgetting policy, which re-validates its own copy — the same indirection
as the reliability policy, so no crate below the host takes a config dependency.
