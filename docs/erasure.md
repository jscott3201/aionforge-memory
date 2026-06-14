# Erasure

How the substrate destroys (05 §3, M5.T03). Erasure is the **one destructive path** in
the system: a hard purge that removes nodes, severs their edges, and clears every index
entry, audited and irreversible. Everything else that retires a memory — forgetting,
supersession, quarantine, demotion — keeps the record; erasure is what you reach for
when the record itself must go. It is off by default behind its own switch, requires a
principal, and refuses whole rather than ever purging part of a cascade.

The sole-writer claim is checkable by inspection: the substrate's delete APIs appear
nowhere in the workspace outside `purge_write.rs` (invariant 13.2 — one grep settles
it).

## The cascade

Erasing a memory erases what was derived from it. The closure walk starts at the seed
and follows *incoming* `DERIVED_FROM` edges (the edge points derivative → source) to a
fixed point, read-only against one snapshot, before any write happens.

Two rules shape the set:

- **Multi-parent survival.** A derivative joins the cascade only when *every* one of
  its sources is already in it. A note summarized from two facts survives the erasure of
  one of them — deleting it would destroy a memory still grounded in a source nobody
  asked to erase. Spared derivatives are reported by id, never dropped silently.
- **Caps refuse whole.** The walk is bounded by a depth cap and a node cap
  (`erasure.max_cascade_depth`, `erasure.max_cascade_nodes`). Exceeding either is a
  typed refusal decided during the read phase; an over-large cascade never opens a
  write transaction, and there is no such thing as a truncated purge.

Each doomed node's `ProvenanceRecord` joins the closure — it is exclusively owned and
carries content about the erased memory, so orphaning it would leave exactly the shadow
record erasure exists to remove. No other edge is traversed: `MENTIONS`, `ABOUT`,
`SUPPORTS` lead to shared ground that must survive.

What one erase leaves standing, deliberately:

| survivor | why |
| --- | --- |
| shared entities | referenced by memories outside the cascade |
| every audit row | the forensic record outlives its subject; only the `AUDIT`/`ATTESTED_BY` edges sever with the node |
| spared multi-parent derivatives | still grounded in a surviving source; named in the report |
| promoted shadows | the cross-namespace `PROMOTED_TO` copy is named in the report, never followed — that boundary belongs to promotion governance |

## Who may erase

Erasure is the one principal-driven surface on the forgetting side. The caller supplies
the acting `Principal`, and the engine's injected `Authorizer` — the same authority
that gates capture and scopes recall — rules on **every namespace the cascade spans**.
One namespace the principal cannot write refuses the whole erasure, before the audit
and the purge: an unauthorized erase reads, but never writes. The refusal names the
namespace that denied.

Two properties fall out of reusing the write-authorization seam rather than keeping a
separate erasure rulebook:

- Under the default policy, `global` and `system` ground is not erasable by any plain
  principal — the same `NotDirectlyWritable` rule that confines capture. Shared ground
  is governed by promotion, not by any one agent's right to destroy.
- Whatever stricter authority a host injects through `with_authorizer` bounds erasure
  automatically.

The protections are inverted from forgetting, on purpose: pinned, attested,
high-importance, referenced — every gate that spares a memory from the reversible sweep
is consulted **nowhere** here. Those gates exist to protect memories from silent decay;
erasure is the explicit escalation they defer to, and it succeeds on a pinned, attested
memory by design.

## The write and its audit

The purge is one transaction: the whole closure is validated before the first row is
removed, every incident edge of every label severs with its node, and all four index
types self-maintain in the same write. A stale closure whose members already died is a
no-op that emits nothing; a partial cascade is never observable.

One `Purge` audit row co-commits with the deletion, addressed to the seed's id in the
seed's own namespace, naming the erasing principal as actor — destruction on an agent's
say-so is attributed to the agent, not the substrate. The payload is counts and a
reason, never content. Deliberately, no `AUDIT` edge is wired: the subject is in the
closure, so an edge to it would be severed by the very deletion it documents. The row
is reachable by its `subject_id` property, which is how the audit-by-subject reads key
every lookup.

Two visibility consequences worth knowing, stated here rather than discovered later:
a refused erasure (unauthorized or over-cap) writes nothing, so it leaves no forensic
trace; and the single purge row lands only in the *seed's* namespace, so another
namespace whose derivative died in the cascade sees no row under its own scoped audit
reads. Both are open policy items for the v1.0 spec pass.

## What "erased" means, honestly

The report says where erased content still physically resides
(`EraseReport.residual_retention`) instead of overclaiming:

- **Live store**: the purge removes the cascade from the graph and every index
  synchronously — search-unreachable at commit. Dead row slots and vector-index
  tombstones remain until `Store::compact()` physically reclaims rows and rebuilds the
  vector index.
- **WAL**: pre-purge property values remain in the write-ahead log. The substrate's
  snapshot publication truncates the log when it runs, but the store does not yet drive
  that pipeline — until it does, this residue has no scheduled eviction.

There is no tombstone registry. A deny-list of erased ids would itself be a record of
what was erased, defeating the point. Re-deriving similar content from *surviving*
sources is lawful by construction — those sources were never erased — and a complete
cascade means the erased subject has nothing left to re-extract from.

## Erasure and forgetting

Separate authorities, separate switches. `erasure.enabled` and `forgetting.enabled`
default off independently; enabling the reversible sweep never stands up the
destructive path as a side effect, and vice versa. A disabled erase surface answers
`Disabled` — the honest answer to a switched-off call, never a fabricated "not found".
A soft-forgotten memory erases like any other: the soft expiry marks it leavable, the
purge makes it gone.

See [Active forgetting](forgetting.md) for the reversible sibling and
[Namespace authorization](namespace-authorization.md) for the authority this surface
reuses.
