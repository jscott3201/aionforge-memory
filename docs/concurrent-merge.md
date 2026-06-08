# Concurrent merge

When several agents write to the same shared memory, their writes have to come together into
one consistent state. Aionforge does this without a separate replication engine: every write
lands in one serialized graph, and the consolidation pass decides how concurrent assertions
about the same thing resolve. The rule that governs that resolution is built so the outcome
does not depend on the order the writes happened to be processed in.

This page covers how a **functional fact** — a fact whose predicate holds at most one value
at a time, like "based in" — converges, and how a **contested belief** — two mutually
exclusive values for the same thing — is resolved. The full type-by-type picture is covered
as the remaining parts land.

## One current value, chosen the same way every time

A functional predicate holds exactly one current object per subject. When two assertions
compete for that single slot — say one episode records "Alice is based in NYC" and another
records "Alice is based in SF" — consolidation has to pick which one is current, and it has
to pick the same winner no matter which episode was consolidated first.

The winner is chosen by a fixed order:

1. **The later event time wins.** Each assertion carries the agent-supplied event time
   (`valid_from`) — when the thing became true, not when the substrate happened to write it.
   The assertion with the greater event time takes the current slot.
2. **A simultaneous tie is settled by the object.** If two assertions share the exact same
   event time — which, for a functional predicate, always means two different objects — the
   tie is broken by the canonical ordering of the object value itself.

Both of these are properties of the assertion alone, so the same set of assertions always
produces the same winner regardless of processing order. The loser is not discarded: it is
**superseded** by the winner, which closes its validity window and links it to the winner but
leaves the fact in place. It stays visible through history and as-of reads — nothing is lost.

### Why the clock is derived, not stored

The order above is computed from values the assertion already carries. Nothing is stamped
with a fresh counter at write time. That is deliberate on two counts:

- A per-write counter would be a second, authoritative copy of derived state, which the data
  model forbids — derived state must be rebuildable from the primary graph, never stored
  alongside it as its own source of truth.
- The substrate replays its write log on recovery and must reproduce byte-identical results.
  A counter incremented per commit would not survive that replay; a value derived purely from
  already-stored fields does.

For the same reason, the order deliberately **avoids** two things it might seem natural to
use. A fact's content-hash id folds in the episode that first asserted it, and the id is
fixed by whichever episode wins the de-duplication race — so it depends on arrival order. The
"originating agent" of a fact has the same problem. Either one, used as a tiebreak, would
quietly reintroduce the order-dependence the whole design is trying to remove. The event time
and the object value do not have that problem, so those are what the order uses. Agent
identity is still recorded for every fact — through its provenance — and is fully queryable
for attribution; it is just kept out of the merge decision, where it cannot converge.

## A backfilled past event still converges

Because the order keys on event time rather than arrival, a stale assertion that shows up
after a newer one does not become a second current value. Suppose "based in SF" (event time
later) is processed first and becomes current, and then "based in NYC" (event time earlier) is
backfilled and processed second. The older NYC assertion is born already superseded by SF: it
takes its place in history with a closed window, and SF stays the single current value — the
same result as if the two had arrived in event order. Processing order changed; the answer did
not.

## Contested beliefs

Some predicates aren't functional but still can't hold two values at once — a thing is either
up or down, not both. When two assertions like that conflict, it's a **contradiction**, and it
is handled differently from a supersession: neither fact is retired. Both are kept, and a
`CONTRADICTS` edge is recorded between them so the conflict can be found and reconciled later.

What recall does have to settle is which of the two it surfaces, and it settles that the same
way regardless of processing order. One side is the **victim**: it keeps its place in the
graph but is held out of recall. The victim is the **lower-trust** side — a contradicting
claim the system is less sure of loses to one it trusts more. When the two are equally
trusted, the tie is broken by the same canonical object order the functional slot uses. Either
way the choice is a function of the two assertions alone — their trust and their values, never
which one happened to be written first — so the same pair resolves to the same surviving value
in any order.

When either side is trusted highly enough, the victim is also **quarantined** — flagged with
an audit signal so the conflict is surfaced for review, not just silently held back. A
higher-trust assertion can quarantine a lower-trust one that was already there, which is the
point: a confident correction should win over a doubtful incumbent, no matter which arrived
first.

One consequence is worth naming plainly. When two equally trusted claims contradict each
other, recall still resolves to one of them, chosen by the object-order tiebreak — a
deterministic choice, but not a meaningful one. The losing claim isn't lost: it stays in the
graph, the `CONTRADICTS` edge records the conflict, and a reader looking to reconcile can find
both. But default recall shows one. Surfacing both equally contested values directly in recall
would be a larger change to how the current-state set is computed, and is left for later.
