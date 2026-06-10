# Drift detection

How the substrate notices the agent moving away from who it said it is (05 §1,
M5.T05). Each core block — persona, commitment, redline — carries an **attested
baseline**: a snapshot of the block's embedding and the namespace's behavior centroid,
co-signed through the same second-attester edit gate that protects the block content.
A periodic detector measures how much farther current behavior sits from each block's
anchor than it did at baseline time, warns through the audit log when a block crosses
the threshold, and never blocks a write. The companion control is the **cooling
window**: a new fact landing close to a high-trust core block is admitted but
rank-sunk for a bounded window, buying the detector time to look before the fact
gains influence.

Everything here is off by default behind one switch (`drift.enabled`), runs on the
host's cadence with the host's clock, and consumes **stored vectors only** — episode
embeddings frozen at write time and the baseline's attested snapshots. The detector
never calls an embedder on the scoring path, so there is no embedder-down condition
there; the one surface that embeds is the baseline-proposal helper.

## Behavior is the episode trace

"Recent agent behavior" is the raw episode stream — written at capture, embedded with
the write-time model, never edited — deliberately *not* the consolidation's derived
facts, which are the poisonable downstream product the detector is guarding. The
per-namespace centroid is the normalized mean of the namespace's recent embedded
episodes (window- and cap-bounded, ascending `(ingested_at, id)` order so a replayed
centroid is byte-identical), restricted to episodes in the live embedder's space.
Fewer comparable episodes than the floor and the namespace's blocks skip — the
detector does not guess from a handful of turns.

## The baseline is the asset

A drift score is only as trustworthy as the anchor it measures from, so the baseline
— not just the block content — is what the attestation gate protects. Seeding and
rebaselining both travel through the attested `edit_core_block` path (content
unchanged, `drift_baseline` replaced): affirming "this behavior is who we are now" is
an identity decision a quorum co-signs, never a detector decision. An un-attested
setter or auto-rebaseline would be the drift-laundering primitive — poison behavior
slowly, move the anchor quietly, and the detector measures distance from the poisoned
anchor. `Memory::compute_drift_baseline` prepares the proposal (the block's current
content embedded under the live model, the current behavior centroid, the integrity
anchors); the quorum makes it real.

The stored schema is versioned JSON on `CoreBlock.drift_baseline`: the mandatory
embedder identity (nothing is ever compared cross-space), a content hash of the block
at baseline time (an attested content edit without a rebaseline reads as
needs-rebaseline, never a score against an anchor that no longer describes the
block), the block-embedding and behavior-centroid snapshots, and the window/sample
provenance. The centroid is nullable: a **genesis baseline** attested before the
namespace had observed behavior scores `0.0` (nothing can drift from behavior never
observed) and is reported as awaiting-first-behavior until a rebaseline arms it.

## Score, threshold, and the sweep

The score is `clamp01(cos(baseline centroid, anchor) − cos(current centroid,
anchor))` — how much farther behavior sits from the attested identity anchor than it
did at baseline. Crossing is per-block (`score ≥ threshold`; an aggregate would hide
a single redline crossing) and a non-finite score never crosses. `Memory::sweep_drift`
walks live core blocks page by page, computes each namespace's centroid once, and
tallies every named skip — `baselines_needed` (the actionable list for attesters),
stale-model (the baseline lives in a different embedding space; a model swap prompts
an attested rebaseline, never an automatic one), awaiting-first-behavior, thin or
unparseable. Skip-never-fabricate: a condition the arithmetic cannot vouch for is a
named count, never a guessed score and never a forced alarm — forcing a maximum score
on measurement failure would train the operator to ignore the one warning channel
that matters.

A crossing commits an `AuditKind::DriftWarning` row in the block's own namespace —
the audit log is the outbox, exactly like the reliability sweep. The row's id is
content-addressed over `(block, baseline epoch, score decile)`, which is the
anti-flap control: re-detecting the same drift against the same baseline dedups to a
no-op, a rebaseline re-arms the warning, and an escalation that climbs into a new
decile warns once more. Warnings never enter recall; the host gates who reads them
through the namespace-scoped audit facade.

## The cooling window

A fact within the proximity threshold of **any** high-trust live core block in its
own namespace is core-proximate and cools: `cooled_until = now + cooling_window`,
stamped once, with a `Cooled` audit row naming the anchoring block. Proximity is
judged against the **attested** `block_embedding` in each block's baseline — the
identity the quorum co-signed, never the block's mutable current vector — and a block
without a baseline anchors nothing. A core block has no subject-predicate-object key,
so agree-vs-contradict is not deterministically decidable over free text; cooling all
proximate facts is the safe over-approximation. An affirming fact loses a few rank
positions for one bounded window and self-heals; a contradicting one is held back
from influence exactly as the spec asks. (An off-by-default LLM tier that narrows
all-proximate to only-contradicting is a flagged follow-up, never on the canonical
path.)

The stamp is **off-cursor by design**. The proximity decision reads baselines that
are written off-cursor, so stamping at materialization — inside the consolidation
pass — would make a replay see different anchors and stamp differently, breaking
byte-identical consolidation replay. Facts therefore materialize with `cooled_until =
None`, and `Memory::sweep_cooling` stamps on the host's cadence, walking
recently-ingested facts by `(ingested_at, id)` watermark. The brief un-cooled gap
between materialization and the next sweep is the same exposure the detector cadence
already accepts; keep the cooling window at or above the sweep cadence so a stamp
outlives at least one detector look.

At rank time the trust re-rank reads a cooled fact's *effective* trust — stored trust
times the cooling factor while `now` sits inside the window — double-gated like
decay: the cooling switch and a caller-stamped clock. The stamp is a separate column
the reliability refold never touches, so the reduction survives a refold; it expires
when the comparison stops applying, with no write and nothing to garbage-collect. See
[Retrieval](retrieval.md).

## Surfaces

| surface | cadence | writes |
| --- | --- | --- |
| `Memory::sweep_drift(after, limit, now)` | host timer | `drift_warning` audit rows only |
| `Memory::sweep_cooling(after, limit, now)` | host timer | `cooled_until` stamps + `Cooled` audit rows |
| `Memory::compute_drift_baseline(block_id, now)` | on demand | nothing — returns a proposal for the attested edit |

All three run at substrate authority and take no principal (drift is maintenance over
the whole identity tier, the forgetting-sweep convention); the host gates who may
call them. Off, they answer empty reports and `Disabled` without touching the graph.
Score and baseline are derived, non-authoritative state — fully rebuildable from
committed episodes plus a fresh attested edit.
