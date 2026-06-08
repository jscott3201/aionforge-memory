# The merge model (CRDTs)

When several agents write to one shared memory, the writes have to settle into a single
consistent state, and they have to settle the *same* way no matter what order they were
processed in. The literature for that problem is conflict-free replicated data types (CRDTs):
data types whose merge is commutative, associative, and idempotent, so replicas that have seen
the same set of updates agree regardless of delivery order.

Aionforge borrows the CRDT *guarantees* without running a replication engine. There is one
store, one serialized write log, and one consolidation pass that resolves concurrent
assertions. This page is the formal companion to [concurrent merge](concurrent-merge.md): that
page walks through what happens to a fact; this one states the model the behavior is built on,
maps each memory type to the CRDT it stands in for, and says exactly what is guaranteed to
converge and what is allowed to lose.

## Why convergence here is just merge determinism

A general CRDT has to converge across replicas that apply updates in different orders and at
different times. Aionforge runs in a smaller setting: a single-writer, strictly serializable
store. Every write is already placed in one total order by the substrate, and consolidation
reads that log.

That collapses the problem. Strong eventual consistency — replicas that have seen the same
updates hold the same state — reduces to one property: **the resolved state is a deterministic
function of the set of assertions, not of the order they were processed.** If two runs that saw
the same assertions in different orders compute the same current state, the system converges.
So the whole job is to make the merge a pure function of the assertion *values*, never of
arrival order, episode id, agent id, or wall-clock time. Everything below is in service of that
one rule.

## The type-by-type correspondence

The spec names four CRDT families. Three map onto mechanisms the store already had; the fourth
is not needed.

### Observed-remove set (OR-set) — fact collections

A set of facts about a subject is an add-wins OR-set. The read surface is the
`current_support_facts` provider over `Fact` nodes, which are content-addressed (a UUIDv8
derived from the assertion) and never deleted.

- **Add** is asserting a fact. The add-tag is the `(subject, predicate, object)` dedup key:
  re-asserting the same triple reuses the existing node instead of making a second one, so the
  add is idempotent — both across episodes and within one episode's replay. The content-addressed
  id is *not* the add-tag: it folds in the source episode and rule version, so it differs from one
  source to the next. Idempotency comes from the dedup lookup on the triple, not from id equality
  — keeping the episode out of the decision is the whole point. Add-wins and idempotent, exactly
  the OR-set property.
- **Remove** is observed-remove: a fact leaves the current set only by a `SUPERSEDED_BY` or
  `CONTRADICTS` edge written against an incumbent the pass actually read. Nothing is removed
  that was not observed, and the node itself stays — it is retired from the current set, not
  deleted, so it is still reachable through history.

Membership converges because it is a function of which nodes exist and which retirement edges
exist, both of which are determined by the assertion set, not its order.

### Multi-value register (MV-register) — contested beliefs

Some predicates can hold only one value at a time but still get two conflicting assertions —
a service is up or down, not both. That is a contradiction, and it is handled like a
multi-value register: concurrent values are *preserved*, not collapsed. Both facts are kept, a
`CONTRADICTS` edge records the conflict, the lower-trust side is held out of default recall,
and when either side clears the trust threshold the held-out side is quarantined with an audit
signal so a human or a later pass can reconcile it. The `unresolved_current` provider surfaces
the open conflicts.

Default recall still shows a single value (which one is covered under the clock below), but the
register keeps every contested value — the MV-register's no-silent-loss promise.

### Last-write-wins register (LWW) — recomputable stats

Last-write-wins is used in exactly one place: the `Stats` block (importance, trust,
last_access, access counts, surprise). Stats is derived and recomputable, so if an update to it
is dropped, nothing of record is lost. That is what makes LWW acceptable here and nowhere else.

Two fields are deliberately *not* in the LWW register:

- `is_pinned` is add-wins, not LWW. A pin is a user instruction; losing it is not acceptable,
  so it does not ride on the loss-tolerant block.
- `Fact.status` looks like a scalar that could be last-write-wins, but it is a redundant mirror
  of edge presence. The `SUPERSEDED_BY` / `CONTRADICTS` edges are authoritative; status is
  recomputable from them, so it is safe by the same recomputable-is-loss-tolerant argument.

What the realized build actually does with stats is narrower than a general LWW register, and
worth stating exactly. Every stats field is written once, when the fact is created, by
`derived_stats`: importance and trust are inherited from the source episode, `last_access` is
the consolidation clock, and the rest start at zero. No consolidation step changes a stats
field after creation — supersession and contradiction touch only edges and the `status` scalar.
So the only way a stats value is ever dropped is a re-assertion of an already-known fact: the
existing fact stands and the re-assertion's stats is never written. That is a first-observed
resolution, and it is order-dependent — whichever assertion arrived first fixes the stats — but
the dropped value is recomputable, so the loss is acceptable by design. That is the line LWW
draws: the set membership converges, the stats register is allowed to lose.

### Sequence CRDT (RGA) — not needed

A sequence CRDT (RGA-style) merges concurrent character edits to shared free text. Aionforge
has no such field. Every free-text surface is one of: immutable and content-addressed
(`Episode.content`, `Note.content`, `Fact.statement`), versioned deprecate-never-delete
(`Skill.body`), or attested as a whole value (`CoreBlock.content`). None is edited
character-by-character by concurrent writers.

Immutable, content-addressed text is a *stronger* no-loss guarantee than a sequence CRDT, not a
weaker one: the text is never overwritten, so it can never be lost or garbled by a merge. A
sequence CRDT is scoped back in only if a future version introduces a genuinely co-edited text
field. Until then it would be a full CRDT wired to nothing.

## The logical clock is derived, not stored

The merge needs an order to pick a winner for a single-valued slot. The spec asks for logical
(Lamport-style) timestamps plus actor ids rather than wall-clock alone. The realized order is a
key computed from fields the assertion already carries:

```
K1 = (valid_from DESC, object_canonical ASC)
```

The assertion with the greatest agent-supplied event time (`valid_from`) takes the slot; a tie
in event time — which for a single-valued predicate always means two different objects — is
settled by the canonical serialization of the object value. Both parts are stable per assertion
and already projected when the merge runs, so the order is a pure function of the multiset
`{(subject, predicate, object, valid_from)}`. Picking the winner is an argmax over an unordered
multiset, which is the same in any order. The contradiction victim is chosen the same way: the
lower-trust object, ties broken by the same canonical object order — a symmetric function of the
contested pair, not "whichever arrived second."

Two things the order deliberately does **not** use, and why:

- **No stored counter.** A per-commit Lamport stamp would be a second, authoritative copy of
  derived state, which the data model forbids, and it would not survive the substrate's
  byte-identical crash-replay. A value derived from already-stored fields survives replay for
  free.
- **No actor id or content-hash id in the key.** A fact's content-hash id folds in the episode
  that first asserted it, and dedup keys on `(subject, predicate, object)` ignoring the episode
  — so whichever episode wins the dedup race fixes the id, and "which is first" is arrival
  order. The originating agent has the same defect. Using either as a tiebreak would quietly
  put arrival-order dependence back into a key whose whole point is to be free of it. The
  "plus actor ids" clause is honored where it belongs — attribution — through provenance: the
  `DERIVED_FROM` edge ties each fact to its source episode and that episode's agent, decoupled
  from the merge key (signing those provenance records is a later part). The key converges;
  provenance attributes.

## What converges, and what is allowed to lose

Pulling the model together:

| Mechanism | CRDT | Converges on | Loss |
| --- | --- | --- | --- |
| `current_support_facts` over `Fact` nodes | OR-set (add-wins) | which facts are current | none — retired, not deleted |
| `CONTRADICTS` + quarantine + `unresolved_current` | MV-register | the set of contested values kept | none — both sides kept |
| `Stats` block | LWW register | nothing required | a re-assertion's stats update; recomputable |
| Immutable content-addressed text | (subsumed) | the text is the id | none — never overwritten |

The single-valued slot (functional predicate, or the one value default recall surfaces for a
contradiction) converges through K1. Everything that is allowed to lose is recomputable, and
`is_pinned` — which is not — is kept out of the loss-tolerant block.

## How the model is checked

The two load-bearing claims — order cannot change the outcome, and nothing is silently dropped
— are tested, not just argued. A property test
(`crates/aionforge-consolidate/tests/convergence.rs`) drives the real consolidation pipeline
over randomized assertion sets, replays each set under several arrival orders that deliberately
disagree with event time, and on every run checks that the recall set is identical across
orders and equal to a winner computed by hand from the values alone, and that the number of
fact nodes equals the number of distinct assertions (a superseded loser or quarantined victim
still counts — it is retired, not deleted). The same suite checks the LWW edge of the model:
every surviving fact's stats, minus the ambient-clock `last_access`, is identical across arrival
orders, and a second consolidation pass over a settled store leaves every stats field
byte-identical. Both the functional path and the contradiction path are covered.

See also: [concurrent merge](concurrent-merge.md) for the operational walkthrough, and
[identifiers](identifiers.md) for how the content-addressed ids the OR-set leans on are built.
