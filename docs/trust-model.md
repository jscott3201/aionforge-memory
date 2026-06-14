# Trust scoring

The substrate keeps a running sense of how reliable each agent has been, and lets that sense shape
what later recalls surface and what can be promoted. An agent that produces facts which hold up
earns trust; one whose facts are contradicted, or whose attestations are later invalidated, loses
it. That score is not a number someone sets by hand — it is folded from a record of what actually
happened.

This is off by default. A deployment that never turns it on keeps every agent at the same neutral
prior, and trust never enters a ranking or a promotion decision. Turning it on is a deliberate
production choice, the same posture as signed writes and quorum promotion.

## Reliability is folded from an event log, not patched in place

Each agent carries a per-category trust map (`Agent.trust_scores`): a category name — usually a
fact's predicate — mapped to a small Beta posterior over "this agent is reliable in this
category." The score is the posterior mean.

The map is a **cache**, not the source of truth. The source of truth is an append-only log of
reliability events, each one an audit record that says "this agent took a success" or "this agent
took a failure," with a fixed weight decided at the moment it was written. To read an agent's
current reliability the substrate folds that log: it sums the success weights into the Beta
`alpha`, the failure weights into the `beta`, and takes the mean. Folding is order-independent
(Beta increments commute) and the events are content-addressed, so a replay or a double-delivery
of the same event folds to the same number. The cache is then rewritten only when the folded value
actually changed.

Folding from a log, rather than nudging the stored number on each update, is what keeps the score
deterministic. The weight rides on the event and never depends on the score at the time, so there
is no read-modify-write race and no path where two updates arriving in a different order leave a
different result. Re-deriving the same decision is always a no-op.

## What moves an agent's reliability

Three things move a score, and they are deliberately asymmetric — it is far easier to lose trust
than to earn it.

- **A contradiction (loss).** When an agent's fact is contradicted by a higher-trust one and
  quarantined, each distinct agent that produced the losing fact takes a full-weight failure.
- **An invalidated attestation (loss).** When a fact is demoted, each distinct agent that attested
  it takes a failure in the fact's category — they vouched for something that did not hold. By
  default this carries the same full weight as a contradiction; the two loss channels are tuned
  independently, so a deployment can weigh one heavier than the other.
- **An agreement (gain).** When a later, independently-authored fact carries what an agent earlier
  asserted, that agent earns a success — but a small one, well under the contradiction weight, and
  only when the corroborating fact was authored by a *different* agent. A producer that also authored
  the corroborating fact is excluded, so an agent cannot vote up its own reliability, and replaying
  the same corroboration folds to a single gain rather than many.

A plain belief revision — an agent superseding its own earlier fact with a newer one — is neutral.
Punishing it would penalize honest updates and hand agents a reason to never correct themselves.

The agreement weight defaults small enough that an agent can never earn back through agreement as
much as one of its own contradicted facts costs it. A deployment that wants a purely punitive
posture can set the agreement weight to zero and ship decay-only.

## A fact's own trust is re-derived, never ratcheted

A fact carries its own trust (`Fact.stats.trust`), and it too is a cache. When an agent's
reliability is refolded, the facts it produced are recomputed: a fact's trust is the lesser of its
**baseline** — the mean of the write-time trust of the episodes it was derived from — and the
reliability of any producer that has *genuinely* decayed below the prior. A producer with no
history, or one that has only gained, is inert and leaves the baseline alone. The outer cap means
reliability can only ever *sink* a fact, never lift it above what it was worth when written, so the
score never ratchets itself up off its own cached value.

The baseline reads the source episodes' immutable write-time trust, not the fact's current cache,
so there is no feedback loop. A single contradiction pins a fact to its producer's true
reliability — a deliberately conservative, hard sink, which the rank-based fusion downstream
treats by position rather than magnitude.

## Reliability can un-promote a fact

Quorum promotion sends a team fact to `global` once enough reliable attesters vouch for it. Trust
scoring adds the missing reverse: a promoted fact whose attesters have since decayed below the bar
is demoted on reliability grounds, even though the original is still perfectly current.

This is the exact complement of the structural demotion that fires when a fact loses support: that
one fires only once the team original has dropped out of the current-support set; this one fires
only while it is *still* current. The two never both apply to one state, and they write their audit
records under distinct tags, so the audit trail keeps a "your attesters went bad" demotion apart
from a "your fact lost support" one. Before the gate re-runs, the attesters are refolded, so the
recomputed posterior reads fresh reliability rather than a stale cache — the verdict never depends
on when the sweep happened to run.

## Trust shapes what recall surfaces

Retrieval treats trust as a **re-rank**, not a search. After the lexical, lexical-anchor, dense,
and associative signals have each surfaced their candidates, trust orders that same set — facts by
their trust, episodes by theirs — and folds that ordering into the rank fusion alongside the others.
It never widens a recall: a fact no other signal found is never pulled in by trust. A low-trust fact
sinks and a high-trust one rises *within* what was already relevant.

The ordering uses a competition rank: candidates with equal trust share a position. That detail
matters because a re-rank covers the whole surfaced set, where many candidates carry the same
score. Spreading equal-trust candidates out by some incidental tiebreak would let trust inject a
bias even when it has nothing to say; sharing a rank means a uniform-trust set adds the same
constant to every candidate in fusion and reorders nothing. Trust only moves a result when the
trust values genuinely differ.

## Off-cursor and host-driven

Every write trust scoring performs — appending a reliability event, refolding a cache, demoting on
decay — happens off the consolidation cursor, never inside a read-only consolidation pass. The
engine exposes the moves as host-driven calls: record a decay or an agreement, refold a set of
agents, sweep a set of promoted candidates for reliability demotion. With trust scoring off, every
one of these is inert.

### The automatic decay sweep

The contradiction decay no longer needs the host to notice each quarantine: the engine can
re-derive it from the record. Every quarantine decision is already a durable, content-addressed
audit row, so the audit log doubles as the work queue — `sweep_reliability_decays` reads the
committed quarantine rows one page at a time, keeps the contradiction quarantines (a governance
demotion-quarantine is the attester channel's business and is skipped), resolves each victim, and
records its producers' decays. Because a decay event's id is content-addressed on the victim and
producer, the sweep mints exactly the id the hand-driven call would: the two paths converge on one
event, never two, and re-sweeping any range is a no-op.

The sweep runs on whatever cadence the host chooses — a timer, session end, a maintenance tool —
and hands back a watermark cursor naming the last row it scanned. Persist it and pass it back to
resume exactly there; lose it and a full rescan is safe, just not free. One caveat on the resume:
rows order by the host-supplied clock, so if that clock ever regresses, a new quarantine can land
*behind* an already-persisted watermark, where no incremental resume will see it. A host that
resumes from the watermark should still run an occasional full rescan — that rescan is the heal
for the clock-regression window, not just the recovery for a lost cursor. The sweep reads the audit
log across all namespaces by design: a quarantine lives in its victim's namespace, and reliability
is a global property of the agent, so a namespace-scoped read would quietly skip decays an agent
has earned. Counts in the sweep's report are true new-event counts — a re-scan over
already-recorded triggers reads back as zero.

One semantic worth naming: the *fact* is the evidence unit, not the quarantine row. A victim
contradicted by three different survivors, or re-quarantined across consolidation cycles, decays
each of its producers exactly once — an agent pays once per wrong fact, not once per time the
system notices the same wrong fact. The attester-side dual (an automatic sweep over demotion
rows) is deliberately not built yet; it needs its own cardinality ruling first.

Two limits are worth stating plainly. First, reliability only moves when an invalidation actually
arrives — a fact that is simply wrong but never contradicted leaves its producer's score
untouched, so trust scoring lowers the cost of bad memory without claiming to find all of it.
Second, the agreement gain rests on the host-asserted authorship of the corroborating fact; the
distinct-author guard, the small weight, and the bounded posterior keep that from being farmable,
and the zero-weight setting removes it entirely for a deployment that would rather not rely on it.
