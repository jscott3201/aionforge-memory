# Namespace authorization

Every memory lives in a namespace, and every write is checked against who is making it.
A capturing agent can only write where it is allowed to, and an attempt to write somewhere
it isn't is refused and recorded. This is the boundary that keeps one agent's private memory
private and keeps shared spaces from being written behind the host's back.

## Namespaces

A namespace is the owning scope of a record:

- **Agent** — an agent's own private space. Each agent has exactly one.
- **Team** — a space shared by the members of a named team. Project or workspace
  sharing uses this same namespace shape, for example `team:aionforge-memory` or
  `team:project-alpha`.
- **Global** — the shared, promoted space every reader can see.
- **System** — internal substrate bookkeeping, never an agent's to read or write.

There is no separate first-class `Project` namespace today. The current policy model
already authorizes shared project memory through host-asserted team membership, and
adding another namespace kind would require a domain enum, schema, client-tool, and
authorization migration. Until that migration has a distinct security need, project
scopes should be represented as named team namespaces.

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

## Refusals and extraction attempts are audited

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

Recall is not refused just because hostile text names another namespace: it still runs under
the normal visible-set filter. But an explicit `agent:<id>` or `team:<id>` token that names a
namespace outside the reader's visible set is recorded as a `namespace_denied` audit before the
search runs. The payload carries the requested namespace, the acting agent, `surface: "recall"`,
and a crafted-query reason; it deliberately does **not** store the query text, which may contain
private material supplied by an attacker.

## Reads

Reads are bounded by the same authority as writes. A recall takes the reading `Principal`
and asks the `Authorizer` for its **visible set** — the global space, the agent's own private
namespace, and the teams it belongs to, never System. A memory surfaces only if its namespace
is in that set; anything outside it is left out of the results rather than returned and
redacted, so a recall never even hints that hidden memory exists.

The visible set is computed once per query, so the check is a single set-membership test per
candidate. Because one `Authorizer` answers both questions, a custom policy injected through
`Memory::with_authorizer` governs reads and writes together: there is no way to widen what an
agent can read without also changing the authority that gates what it can write.

A surface that drives a recall supplies the reader's teams the same way it supplies the reader.
The MCP `search` tool, for instance, takes the viewer agent plus an optional `teams` list the
host asserts for that reader, and builds the `Principal` from both; an omitted list leaves the
reader scoped to the global space and its own private namespace. A capture over that same
surface is untrusted, so it is confined to the writer's private namespace regardless of any team
the caller might name — team-shared writes are a trusted-host path, not a remote-client one.

## Injecting a policy

`Memory::new` installs the default policy. A host that needs a different one — say, signature
gating on top of namespace rules — supplies its own through `Memory::with_authorizer`. The
`Authorizer` is the single seam every write is checked against, so a custom policy governs the
whole capture path, not just part of it — and the [erasure cascade](erasure.md) too, which
demands write authority over every namespace it would destroy in.
