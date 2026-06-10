# Core memory

Core blocks are the identity tier: the agent's stable self-description (`persona`),
its standing promises (`commitment`), and its inviolable constraints (`redline`).
They are the most strongly protected memories in the substrate, because they are the
ones an attacker — or a slowly drifting agent — would most like to rewrite.

The protection model is one rule with teeth: **a single writer cannot edit its own
identity.** Every edit needs at least one verified attester who is not the editor,
and sensitive blocks can be configured to require a human one. There is no off
switch. An unconfigured deployment gets the spec's floor (one non-editor attester),
not an exemption.

## One block, one id, for life

A core block is one node with one stable id. An edit replaces the content **in
place** — whole-value, never merged — and the block's history lives in its signed
`core_edit` audit trail, which records the prior and new content hashes of every
transition. There is no version chain in the graph: erasing the id destroys the
whole block with nothing orphaned, and nothing stale left behind to recall.

Three consequences worth knowing:

- **The drift baseline carries forward.** An ordinary edit never re-baselines
  `drift_baseline`; updating it is the drift detector's privileged call (M5.T05). If
  an edit could move its own baseline, drift would launder itself past detection.
- **A stale embedding is removed, not served.** If an edit supplies no fresh
  embedding, the old vector is deleted — it indexes text the block no longer says.
- **A retired block stays retired.** Editing a block whose `expired_at` is set is
  refused; a retired identity is not edited back to life.

## The edit gate

An edit is **one call**, host-coordinated. The host collects the editor's and the
attesters' signatures out of band and presents them together; the substrate verifies
all or refuses all, atomically. There is no pending-edit state inside the substrate —
a browse-pending surface is exactly the trust-laundering path the spec forbids.

The gate refuses everything refusable before the store write, in this order:

1. **Namespace authority.** The injected `Authorizer` rules on the editor's write
   authority over the block's namespace first. Attesters vouch for *content*, never
   for *authority* — an agent outside the namespace cannot edit its identity no
   matter how many votes it brings. Refusals land a `namespace_denied` audit in the
   system namespace, like every cross-namespace write attempt.
2. **The editor leg** (when signed writes are on). The editor proves key possession
   over the block id and instant. With signed writes off, editor identity rests on
   the host-asserted principal: the behavioral single-writer rejection still holds,
   but the *cryptographic* editor-exclusion guarantee requires `signed_writes`.
3. **The votes.** Each attester signs the canonical core-edit payload over the
   block's stable id **and the exact prior-to-new content transition**, at an
   instant inside the clock-skew window. A vote authorizes one specific replacement
   of one specific block — a voucher collected for one proposed edit can never
   validate different content. (A fact's content-addressed id binds an attestation
   to content by itself; a core block's deliberately stable id cannot, so the
   transition rides in the signed bytes.) One forged vote refuses the whole edit.
4. **The count.** Distinct verified non-editor attesters must meet the block's
   requirement. The editor is excluded by id, never counted; duplicates collapse.
5. **The human requirement.** If the resolved rule demands it, at least one verified
   vote must come from an agent on the deployment's certified-human list whose
   `Agent` row is still `Active`. The store re-checks that status under the same
   write lock as the swap, so a reviewer retired mid-flight fails closed.

The store then re-checks the prior-content hash under its write lock (compare and
swap): if a concurrent edit landed first, the whole edit is the typed
`StaleContent` refusal — applying it would record votes against bytes that were
never the actual predecessor.

**Every gate rejection is audited** in the block's own namespace, with the principal
as actor, the reason, and the refused transition's hashes. A refused self-edit is
not noise; it is precisely the sycophantic-drift signal the threat model wants on
the record.

## Strictness, per block

The requirement a block resolves to composes **strictest-per-axis** from three
sources: the default rule, the rule keyed by the block's `sensitivity` string, and
the implicit redline rule. The attester count takes the maximum; the human flag
takes the OR. A sensitive block is never edited under a laxer bar than any rule
that applies to it.

Humanness is a host policy assertion, not a substrate-verifiable fact: the
deployment certifies specific agent ids as human-controlled keys in its
configuration. A human attester's vote verifies like any other; the gate
additionally requires the id to be on the list and the agent to still be active.

## Creation and reads

Creation is the **un-attested** half of the contract. A genesis block has no prior
identity to drift from, so the gate on creation is namespace authorization plus the
genesis audit — one commit writes the node and its `core_edit` row together, and the
`UNIQUE` id makes a duplicate create fail whole. Under the default policy that
yields a useful derived property: nobody can create (or edit) global or system core
blocks, because those namespaces are not directly writable.

Reads are scoped by the principal's visible set, like every read surface. A block
outside the principal's view reads as absent, indistinguishable from one that does
not exist.

## Recall: identity is always in context

Every recall prepends the live core blocks the reader can see, ahead of the ranked
results. They do not compete on relevance — identity is the standing context a
recall is read against, not a search hit — so they bypass fusion and the
session-diversity cap, carry an `always="true"` marker instead of a score, and are
ordered by their content-derived serialization id so the rendered view stays
byte-identical across calls.

The prefix counts toward the requested limit (the ranked fill shrinks to make room)
but is never itself capped: a deployment with more identity than limit still gets
all of it rather than a silent truncation of a redline. The same visible-set rule
gates the prefix as every other read, and a retired or soft-forgotten block is
simply not live, so it does not surface. Rendered core blocks sit inside the same
escaped `recalled-memory-context` wrapper as every recalled memory — identity
content is still third-party data to the model reading it.

## Configuration

```toml
[core_block]
redline_requires_human = true
human_attester_ids = ["0197b0aa-3c5e-8000-8000-000000000000"]

[core_block.default_rule]
k = 1                      # distinct non-editor attesters; >= 1, no off switch

[core_block.rules.pii]     # keyed by the block's sensitivity string
k = 2
require_human = true
```

Validation is fail-closed at startup: a zero `k` anywhere is refused (a quorum of
none would re-enable single-writer edits), and any human requirement with an empty
`human_attester_ids` is refused (an unsatisfiable gate would brick every sensitive
edit). Because the always-on gate consumes `security.clock_skew_tolerance_ms`, that
window is validated unconditionally too — a zero window would silently refuse every
identity edit, so it is a configuration error instead.

The host maps `CoreBlockConfig` into the engine's `CoreEditPolicy` field for field,
the same indirection as every other policy: no crate below the host takes a config
dependency, and the engine re-validates its own copy at construction.

## Accepted residual

Replay defense is the skew window plus the transition binding plus the compare and
swap. The one residual: if a block is edited back to its exact prior bytes inside
the skew window, an unexpired vote for that same transition could re-apply. The
window is short, and what re-applies is byte-for-byte what was vouched for — an
attacker gains no content they did not already have attested.
