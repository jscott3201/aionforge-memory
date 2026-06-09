# Documentation

System documentation for Aionforge Memory — how the pieces work and how to use them.
This is reference and guides, not planning or changelogs.

## Subsystems

- [Procedural memory](procedural-memory.md) — skills stored as data: versioning,
  reliability, reliability-weighted retrieval, bad-pattern avoidance, and conservative
  off-by-default skill induction.
- [Completion client](completion-client.md) — the optional, off-by-default chat client:
  one provider-agnostic seam over OpenAI Chat Completions, OpenAI Responses, and Anthropic
  Messages (and any OpenAI-compatible local server), with pinned sampling and graceful degrade.
- [LLM distillation](distillation.md) — the optional, off-by-default layer that condenses facts
  into notes with a chat model, run off the consolidation cursor so it can never perturb the
  byte-deterministic canonical path; guarded against lossy output and degrading to the rule tier.
- [Note link evolution](link-evolution.md) — the optional, off-by-default layer that draws and
  revises bi-temporal `RELATES_TO` edges between notes with a chat model, off the cursor and behind
  a closed relationship vocabulary, a confidence floor, and per-run cascade caps; degrades to a
  deterministic rule tier.

## Boundaries

- [Namespace authorization](namespace-authorization.md) — who can write where: the caller-asserted
  principal, the own-private / member-team write policy, refused-and-audited denials, and the
  visible set that bounds reads.
- [Provenance signing](provenance-signing.md) — the off-by-default signed-write gate: the host signs
  and the substrate verifies, the host-supplied episode id and its collision guard, writer enrollment,
  the clock-skew window, and the audited refusals — all with the unsigned path untouched.
- [Attestation and quorum promotion](attestation-and-promotion.md) — the off-by-default path a team
  fact takes to global: explicit signed attestations, the sybil-bounded reliability-weighted posterior,
  the count-and-threshold gates, the promoted global copy and its ledger, and the demotion that
  quarantines the copy on lost support while leaving the namespace original untouched.

## Consolidation

- [Concurrent merge](concurrent-merge.md) — how concurrent writes about the same thing come together
  into one state: a functional fact converges to a single current value chosen by event time then
  object, so the outcome does not depend on processing order, and the loser is kept in history.
- [The merge model (CRDTs)](crdt-model.md) — the formal companion to concurrent merge: which CRDT
  each memory type stands in for (add-wins set, multi-value register, last-write-wins stats), why
  convergence here is just merge determinism, and why the logical clock is derived, not stored.

## Substrate

- [Identifiers](identifiers.md) — how ids work: time-ordered UUIDv7 for generated records,
  deterministic UUIDv8 for content-addressed ones, stored as native UUID values.

More subsystem guides land here as each one is built.
