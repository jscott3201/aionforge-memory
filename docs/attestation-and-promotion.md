# Attestation and quorum promotion

A memory written in one team's namespace stays there until other agents vouch for it. Quorum
promotion is the one path a team fact takes to the shared `global` namespace, and it is gated:
a fact promotes only after enough independent agents sign an attestation for it and the
substrate's confidence in it clears a threshold. Demotion is the reverse, and it never destroys
the original.

This is off by default. A single-team or development deployment never promotes, with no
overhead. Turning it on is a deliberate production decision.

## Attestation is explicit and signed

An attester vouches for a fact by signing it. The signature is Ed25519 over the same kind of
canonical, versioned payload the provenance signatures use — here over the fact id, the
attester's id, and the attestation time — and the substrate verifies it against the attester's
registered public key. It only ever verifies; a private key never enters the substrate. A
malformed or wrong-key signature is refused, and an attestation whose timestamp sits outside the
clock-skew window is refused too (the same replay window signed writes use).

Attestation is **explicit**: an attester must already know the fact's id. There is no surface
anywhere that lists pending candidates to browse. That closes a laundering path — a private fact
can't be quietly walked toward global by attesters who don't know what they're vouching for.

An attester votes once. A second attestation from the same agent records nothing new and never
rewrites the first: the attestation edge is immutable, and one agent is one vote.

## How the substrate decides

The substrate weighs each attester by how reliable that agent has been, then asks how confident
those weighted votes make it that the fact is correct. The confidence is a Beta posterior: each
attester contributes its reliability `r` (a number in `[0, 1]`) as evidence — `r` toward correct,
`1 - r` toward not — over a prior.

The shape of that posterior matters. Because every attester adds exactly one to the denominator,
the confidence **settles toward the average quality of the attesters and can never be pushed to
certainty by sheer numbers**. A crowd of merely-above-average attesters can't manufacture a high
posterior; only genuinely reliable agents move it. That is the property that keeps a flood of
low-quality or sock-puppet votes from promoting anything (07 §T5, and the red-team requirement
that no malicious skill reach global through quorum).

Two gates both have to clear, and neither trades off against the other:

- **The count.** At least `k` distinct attesters, where `k` is at least two — a quorum of one is
  not a quorum.
- **The posterior.** At or above the category's threshold.

Because the posterior is bounded by attester quality, the threshold is a real bar — reached only
by a strong consensus, not by a few votes. But the count and the threshold can't be set apart from
each other. The same bound that stops a flood also caps what any fixed quorum can reach: `k`
attesters, however reliable, can't push the posterior past `(alpha + k) / (alpha + beta + k)`. So
the default pairs a quorum of three with a `0.80` threshold — the highest a three-attester quorum
can clear under the default prior — and the substrate refuses a policy whose threshold its own `k`
can never reach rather than quietly promoting nothing. A deployment that wants a stricter global
bar raises the count and the threshold together. When a candidate's attestations span more than one
category, the effective gate takes the **strictest count and the strictest threshold
independently**, so a fact touched by a sensitive category is never promoted under a laxer bar (and
the bar can end up stricter than any single category's rule).

## Reliability comes from elsewhere

This layer **reads** an agent's reliability; it never changes it. Maintaining per-agent
reliability — raising it when an agent's attestations hold up, lowering it when they're later
invalidated — is [trust scoring](trust-model.md)'s job, which builds on this one. Until an agent
has been scored it
contributes the uninformative `0.5`, which moves the posterior toward neither pole. So on a cold
start, before any reliability has been earned, nothing promotes — the conservative default.

> **A note on independence.** "Independent attestations" here means distinct attesters: one signed
> vote per agent. Fully excluding a fact's own author from its quorum would need an authorship link
> the substrate doesn't yet record, so that refinement waits on it. Until then an author's vote
> fills one of the `k` slots like any other, so independence rests on the quorum of at least two
> plus the reliability-weighted posterior — a lone author still can't promote its own fact, and a
> low-reliability one barely moves the bar.

## Promotion writes a copy, never a move

Only a fact that still holds can promote. If it has been superseded by a newer assertion or
contradicted — so it has dropped out of the current-support set — its standing is gone, and the
old attestations on it can't carry it to `global`. Promotion is the exact mirror of the demotion
below: the same lost-support test that pulls a promoted fact back is what blocks an unsupported one
from going up in the first place.

Promoting a team fact creates a **new** fact in the `global` namespace — a copy that carries the
same statement, subject, and embedding — and links the original to it with a `PROMOTED_TO` edge.
The team original is left exactly as it was. The global copy's id is derived from the original's
id, so promoting the same fact twice lands on the same copy: a no-op, not a duplicate. A
`Promotion` ledger entry records the posterior, the count, and the outcome, and the promotion is
audited.

## Demotion quarantines the copy and keeps the original

A promoted fact loses its standing when the team original drops out of the current-support set —
when it has been superseded by a newer assertion or contradicted. On that lost support the
substrate **demotes**: it links the global copy back to the original with a `DEMOTED_FROM` edge,
quarantines the copy (it is expired and marked quarantined, so it falls out of live recall), and
flips the ledger to rejected. The team original is left untouched, and both the demotion and the
quarantine are audited.

Demotion is reversible and never destructive. The global copy isn't deleted — it's set aside —
and because its id is stable, support regained later re-promotes onto the same node. (Demotion
driven by an attester's reliability decaying, rather than by the original losing support, arrives
with trust scoring; it reuses this same demotion machinery.)

## Turning it on

Promotion is controlled by a small policy:

- `promotion.enabled` — off by default; set it on to gate promotion.
- `promotion.default_k` / `promotion.default_threshold` — the count and posterior bars, bounded to
  a quorum of at least two and a threshold in `(0.5, 1.0]` that the count can actually reach under
  the prior. The default is a quorum of three at `0.80`; a higher threshold needs a higher count.
- `promotion.prior_alpha` / `promotion.prior_beta` — the Beta prior; the default `1, 1` is
  uninformative.
- `promotion.default_category` — the bucket an uncategorized attestation falls into.
- `promotion.categories` — per-category overrides, where a sensitive category sets a stricter
  count and threshold.

When promotion is off there is no orchestrator and no crypto on the path: the attestation API is
inert, exactly as the capture path is unsigned when signed writes are off.

A refused attestation writes no memory but doesn't vanish: it commits a single audit event and
returns a deliberately coarse error. An unknown attester, a bad signature, and an attestation for
a fact the caller couldn't name all come back as one refusal, so the substrate is neither an
enrollment oracle nor a forge oracle; a clock-skew refusal is reported on its own so an honest
client knows to resync. A backend read failure while resolving a key is an availability fault, not
an attack, and is surfaced without a security audit.
