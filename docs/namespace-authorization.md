# Namespace authorization

Every memory lives in a namespace, and every write is checked against who is making it.
A capturing agent can only write where it is allowed to, and an attempt to write somewhere
it isn't is refused and recorded. This is the boundary that keeps one agent's private memory
private and keeps shared spaces from being written behind the host's back.

## Namespaces

A namespace is the owning scope of a record:

- **Agent** — an agent's own private space. Each agent has exactly one.
- **Team** — a space shared by the members of a named team.
- **Global** — the shared, promoted space every reader can see.
- **System** — internal substrate bookkeeping, never an agent's to read or write.

## The principal

Authorization is decided against a `Principal` — the identity the host asserts for a call:

```rust
Principal::new(agent_id, teams)   // the agent, and the teams it belongs to
Principal::agent(agent_id)        // an agent with no team memberships
```

The host authenticates the caller and asserts its team memberships; the substrate trusts
that assertion in-process and does not keep its own membership graph. That is a deliberate
choice. The alternative — storing membership as edges and re-reading them on each write —
opens a gap between the check and the write (the membership could change in between) and
makes the substrate the source of truth for something the host already knows. Keeping the
principal caller-asserted closes that gap: the identity is fixed for the duration of the
call. Empty team names are dropped when a principal is built, so a blank string can never
stand in for a real team.

## What a write is allowed to do

The policy, applied by the `Authorizer` seam before a capture commits anything:

- A write may target the agent's **own private** namespace.
- A write may target a **team the principal is a member of**.
- **Global** and **System** are never written to directly. Global is reached only through
  promotion (a separate, deliberate path); System is the substrate's own.

A write that asks for somewhere it isn't allowed is **refused, not silently downgraded** —
with one exception below. The check runs *before* content de-duplication and before any node
is written, so a forbidden write never touches the graph.

### Trusted versus confined requests

A request carries a `trusted` flag the host sets when it vouches for the caller's asserted
namespace. The two cases differ in how a team request that the principal can't satisfy is
handled:

- A **trusted** request to a non-member team, or to Global or System, is **refused** — the
  host asked for a specific place the principal has no claim to, so the substrate declines
  rather than guess.
- An **untrusted** request that names a team is **confined** to the agent's private space
  rather than refused. An untrusted caller doesn't get to place memory in a shared space on
  its say-so, but its write still lands where it unambiguously belongs — its own.

## Refusals are audited

A refused write writes no memory, but it does not vanish. The substrate commits a single
`namespace_denied` audit event in its own transaction:

- its `kind` is `namespace_denied`,
- it lives in the **System** namespace,
- its subject and actor are both the acting agent,
- its payload carries the requested namespace and the reason for the refusal.

No `AUDIT` edge is wired from it, because a rejected write produces no memory node for the
edge to point at — the subject is the agent itself. The event is found instead through the
scalar `kind` and `subject_id` indexes, so an agent's denied attempts can be listed by
subject.

## Reads

Reads are bounded by namespace as well. A recall returns memory from the global space and
from the viewer's own namespace; another agent's private memory never surfaces — it is left
out of the results rather than returned and redacted.

The `Authorizer` also computes a principal's **visible set** — the full read boundary the
policy intends: the global space, the agent's own, and its member teams, never System. That
is the same shape as the write rules above. The recall path scopes reads itself today (global
and own namespace); routing it through this visible set, so a viewer reads from its member
teams too, is the matching read-side step that lands alongside team-aware retrieval.

## Injecting a policy

`Memory::new` installs the default policy. A host that needs a different one — say, signature
gating on top of namespace rules — supplies its own through `Memory::with_authorizer`. The
`Authorizer` is the single seam every write is checked against, so a custom policy governs the
whole capture path, not just part of it.
