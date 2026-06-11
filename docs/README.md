# Documentation

System documentation for Aionforge Memory — how the pieces work and how to use them.
This is reference and guides, not planning or changelogs.

## Reading and writing

- [Capture](capture.md) — the fast write path: the privacy/injection filter and the origin
  block, exact-then-near dedup, the ADD-or-nothing decision, embedding and provenance, and the
  single durable-before-visible commit.
- [Retrieval](retrieval.md) — native hybrid recall: BM25 and vector (with exact rerank) through
  the engine, RRF fusion, the query-class router, the high-precision default path, and the
  deterministic dual-view recall bundle.
- [Graph signals](graph-signals.md) — the two associative signals: seeded Personalized PageRank
  over query-mention entities, and graph-expanded support scoring, each gated to the query classes
  it helps.

## Memory over time

- [The bi-temporal model](bi-temporal-model.md) — event time versus transaction time, the four
  retrieval modes (current / as-of / as-known-at / history), non-destructive supersession and
  contradiction, and the maintained current-state providers rebuilt from the primary graph.
- [Consolidation](consolidation.md) — the asynchronous, deterministic pipeline that turns raw
  episodes into facts and notes: the durable cursor and crash-safe scheduler, fact extraction and
  entity resolution, supersession/contradiction detection with quarantine, and non-lossy summarization.
- [Concurrent merge](concurrent-merge.md) — how concurrent writes about the same thing come together
  into one state: a functional fact converges to a single current value chosen by event time then
  object, so the outcome does not depend on processing order, and the loser is kept in history.
- [The merge model (CRDTs)](crdt-model.md) — the formal companion to concurrent merge: which CRDT
  each memory type stands in for (add-wins set, multi-value register, last-write-wins stats), why
  convergence here is just merge determinism, and why the logical clock is derived, not stored.
- [Decay and importance scoring](decay-and-importance.md) — how relevance ages: per-tier
  exponential half-lives over a pure, never-written-back effective importance, the pin that
  never decays out of eligibility, the caller-supplied clock (the MCP server stamps it; the
  substrate reads none), and the importance/recency re-ranks in three-factor relevance.
- [Forgetting](forgetting.md) — the conservative, default-off, reversible soft-expiry: one
  bare `expired_at` among four orthogonal lifecycle signatures, the spare-only eligibility
  axes and graph protections, the watermark sweep and fully-gated point ops, the
  always-available audited pin/unpin (a stay, not a vault), the decision-basis audit
  trail in the memory's own namespace, and the single retrieval gate with
  `include_expired` as the one retention flag.
- [Erasure](erasure.md) — the one destructive path: the fixed-point `DERIVED_FROM`
  cascade under the multi-parent survival rule, caps that refuse whole, write authority
  demanded over every namespace the cascade spans, the principal-attributed purge audit,
  what survives on purpose (entities, audit rows, promoted shadows, spared
  derivatives), and the honest residency boundary — compact-reclaimed rows, a WAL with
  no scheduled eviction yet, and no tombstone registry.
- [Core memory](core-memory.md) — the always-on identity tier: one stable id per block
  with history in the signed audit trail, the second-attester edit gate with
  transition-bound votes (a voucher authorizes one exact replacement, never "some
  edit"), strictest-per-axis sensitivity rules, the certified-human requirement
  re-checked under the write lock, and namespace authority ruled before any vote is
  weighed — attesters vouch for content, never for authority.
- [Drift detection](drift.md) — how the substrate notices the agent moving away from
  who it said it is: attested baselines as the protected asset (seed and rebaseline
  both quorum-co-signed; an un-attested setter is the laundering primitive), the
  episode trace as the behavior signal, per-block scoring with every skip named and
  nothing fabricated, content-addressed anti-flap warnings through the audit outbox,
  and the off-cursor cooling window that rank-sinks core-proximate facts while the
  detector gets its look.
- [Cross-family consolidation guard](cross-family-guard.md) — why a consolidating
  model never condenses its own family's writing: the comparison-time family
  normalization (boundary prefix, closed vendor roots, fail-closed on anything
  unverifiable), the per-item enforcement inside distillation and link evolution
  with the mode fixed at construction, the two-hop launder closed by unioning a
  note's own authoring model into its writer set, the single-family startup
  warning, and the one-call lineage read.

## Procedural and generative layers

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
- [Trust scoring](trust-model.md) — the off-by-default reliability layer the line above reads from:
  per-agent reliability folded from an append-only event log, the asymmetric loss/gain weights, the
  recomputable fact-trust cache, the reliability-decay demotion that complements lost-support, and
  the competition-ranked trust re-rank that orders recall without widening it.
- [The audit subgraph](audit-subgraph.md) — the forensic record every governance operation leaves:
  the single write funnel and its blank-to-signed signature latch, off-by-default substrate audit
  signing with file or env seed custody, the out-of-band keyring anchor and its genesis/heal protocol,
  per-row verification verdicts on the read surface, and the operator runbook.
- [Red-team suite](red-team.md) — the CI-gated security probes and their structured reports:
  M6.T04's query-only injection, poisoned-RAG recall, and malicious-skill promotion probes,
  with the full-denominator reporting convention later M6/M8 gates reuse.

## Substrate

- [Identifiers](identifiers.md) — how ids work: time-ordered UUIDv7 for generated records,
  deterministic UUIDv8 for content-addressed ones, stored as native UUID values.

More subsystem guides land here as each one is built.
