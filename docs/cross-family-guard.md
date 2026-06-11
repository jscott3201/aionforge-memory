# Cross-family consolidation guard

How the substrate keeps a consolidating model from condensing its own family's
writing (07 §3, M6.T01). Behavioral traits transmit through distillation when the
model doing the condensing shares a base model with the writers whose content it
reads — mixing unrelated data reduces the effect but does not eliminate it, and
cross-family distillation suppresses it. So every consolidation rule that calls
inference — the LLM distiller and the LLM link evolver, both off by default —
verifies before each model call that the consolidating family differs from the
writers' families. A same-family or unverifiable item is refused (the default) or
condensed with a warning, per config, and either way the finding lands in the audit
log as a `subliminal_guard_warning` row.

The guard is enforced at the substrate, not left to user code: the mode is set once
on `MemoryConfig.consolidation_guard` at construction (`[consolidation_guard]` in
the config file), the facade threads it into the drivers, and the consolidating
family is read from the summarizer's or evolver's own declared identity — the same
identity the provenance audit records, so a caller cannot hand the guard a
different story than the audit trail sees. A deterministic rule implementation
declares no model family and is outside the guard's scope entirely; nothing was
inferred, so nothing can transfer.

## What counts as the same family

"Family" arrives as free text on both sides — the writer's family is asserted by
the host at capture, the consolidator's is the completer's configured model id —
so the comparison normalizes at comparison time and never rewrites stored
provenance. In order:

1. **Trim and lowercase.** Empty or whitespace on either side is *unverifiable*,
   never "different".
2. **Hyphen-boundary prefix, either direction.** `claude` and `claude-sonnet-4-6`
   are the same family; `claude` and `claudex` are not a match (the boundary is
   anchored, so a bare character prefix proves nothing).
3. **A closed vendor-root table.** `gpt-5` and `o3` share no prefix but share a
   vendor lineage; the table maps known leading tokens to a root (claude →
   anthropic; gpt/o1/o3/o4 → openai; gemini/gemma → google; llama → meta;
   mistral/codestral → mistral). The table is closed and amended under the same
   discipline as a closed enum.
4. **Leading-token equality** for vendors the table does not know — `deepseek-r1`
   and `deepseek-v3` compare the same; two distinct unknown vendors compare
   different.

The rule fails closed: any prefix or shared-root relation resolves to *same* (the
riskier path), and ambiguity resolves to *unverifiable*, which fires the guard just
like a match — 07 §3 rejects auto-routed, unverifiable model identity precisely
because an unverifiable family breaks both the guard and lineage. The comparator
can only be as honest as the recorded writer family, though: a host that asserts a
misleading alias can false-pass as cross-family. That limit belongs to the
honest-scope statement, and the guard's job is to make the *recorded* identities
tell one consistent story.

## Who the writers are

For a distillation cluster, the writer set is resolved per source fact through
`Fact -DERIVED_FROM-> Episode` and a fail-closed chain: the episode's
`ProvenanceRecord.model_family` first (the signed write-proof; its value is final
even when empty — falling through past a signed empty family would let a later,
mutable declaration launder an unverifiable write), the episode's `origin` copy
when no record exists, and the agent's current declaration last. A fact with no
source episode, a chain that dead-ends, or a recorded-empty family marks the whole
set *unverifiable* — one source nobody can vouch for poisons the item, however
many others resolved.

For link evolution, a source note's writers are that same chain through the note's
sources, **unioned with the model that authored the note itself** (read from its
`distill` audit). Without the union there is a two-hop launder: distill with
family X (producing a note that carries X's traits), then evolve links with X and
pass because the note's underlying episode writers were some other family. A note
with no author evidence at all — no lineage, no distill audit — is unverifiable,
not unauthored.

Within a run, the guard applies per cluster (distill) or per source note (link
evolution) and skips the offending item rather than aborting the call: one
same-family cluster must not deny distillation of cleanly cross-family clusters.
Refusals are counted in the run report (`guard_refused`) and audited; warn-mode
findings proceed and are visible in the audit trail alone.

## The audit trail

Every fired finding writes one `subliminal_guard_warning` row whose payload names
the action (`refused`/`warned`/`startup`), the rule (`distill`/`link_evolve`), the
consolidating family, the resolved writer families, the reason (`same_family`,
`unverifiable_writer`, `unverifiable_consolidator`, `single_family_deployment`),
and the matched writer family when there is one. The row id is content-addressed
over the finding — not the instant — so a re-run over unchanged ground dedups to
the same row instead of flooding the log, while a changed mode, writer set, or
fleet records a new finding.

## The startup warning

A deployment whose every enrolled agent declares the consolidating model's own
family is exactly the posture the guard exists to flag, so construction checks for
it: when `consolidation_guard.declared_consolidator_family` is set (populate it
from `completer.model` when distillation is in use), the engine reads the distinct
agent families and — if all of them compare *same* against the declaration —
surfaces `StartupWarning::SingleFamilyDeployment` through
`Memory::startup_warnings()` and writes the audit row. The engine itself never
logs; the host renders the warning. The check is best-effort by design (no
declaration, a mixed fleet, or an empty store skips it) and restarts dedup to one
audit row; the per-call guard is the guaranteed enforcement either way.

## Querying lineage

`Memory::note_lineage(&note_id)` answers "where did this note come from" in one
call: the source facts and episodes (the `DERIVED_FROM` walk), the model that
authored it (from its `distill` audit; `None` for a deterministic rule summary),
the writer families behind its sources, and an explicit `non_canonical` marker —
a note never enters the deterministic current-fact path, so the acceptance
property is queryable rather than implicit. It is a point read: the producing
model lives in audit payload, not an indexed column, so filtering notes *by*
model family should drive the guard surface instead.

## Spec gaps this design filled

The spec mandates the guard but leaves four things unnamed, all filled here and
flagged for owner ratification: the family-equality semantics (the normalization
above — 07 §3 never defines "family"); the refuse-or-warn config key and its
`refuse` default; the mixed-corpus rule (skip the offending item, not the whole
call); and the writer-set aggregation (any-match over the union — the spec says
"the originating writer's family", singular). The reuse of the one reserved
`SubliminalGuardWarning` audit kind for refusals (discriminated by payload, no
closed-enum amendment) and the LLM link evolver's inclusion (07 §3 says "every
rule that calls inference"; the task's dependency list named only distillation)
are likewise recorded design decisions, not spec text.
