# Procedural memory

Procedural memory is where an agent keeps the procedures that worked — skills — so it
can reuse them instead of working a solved problem out from scratch every time. A skill
is stored as data, never executed by the substrate; the agent that retrieves it decides
whether and how to run it.

> **Pre-alpha.** The behavior described here is settled design, but signatures and
> defaults can still move. Check the types in `aionforge-domain` and `aionforge-procedural`
> for the current surface.

## What a skill is

A skill has a stable `name`, a monotonic `version`, a `body` (the procedure itself, as
text), and the contract around it: declared `capabilities`, a parameter schema, optional
pre- and post-conditions, and a `language` tag. It carries a "what problem does this
solve" embedding and a human-readable `description`, which are the two surfaces retrieval
searches. It also keeps a running success/failure count — its track record.

The substrate **deprecates, never deletes**. Saving a new version stamps the prior
active one with a `deprecated_at` time and leaves it in place, so the full history stays
queryable and at most one version per name is ever active.

## Saving a skill

`save_skill` takes a whole skill and decides what to do with it:

- **Change detection.** If the body and the frozen contract surface (capabilities
  compared as a set, params, pre/post-conditions, language) all match the active
  version, the save is a no-op and returns the existing skill's id. Re-registering an
  unchanged skill on every startup never churns the version history. The `description`
  is deliberately left out of this check — it is a recall surface, not part of the
  procedure, so editing it alone does not cut a version.
- **A new version** is cut when any of that surface changes. It gets the next version
  number (one past the highest ever recorded for the name), the prior active version is
  deprecated in the same atomic commit, and reliability **resets to zero** — a changed
  body is a different procedure whose track record has to be earned again.
- **Audit trail.** Every save writes a `SkillSave` audit. A version bump also writes a
  `SkillDeprecate` for the superseded version and a `SkillVersionDiff` that records the
  capability changes (added and removed) and whether the body changed. The diff is how
  you see, after the fact, exactly what one version changed about the one before it.
- **Embedding.** If the skill arrives without a problem embedding, the layer computes
  one from the description through the configured embedder and records which model
  produced it. If the embedder is down and no embedding was supplied, the save **fails
  closed** rather than store a skill the vector index can never surface — saves are not
  on a latency-critical path, and a silently unretrievable skill is worse than a save
  the agent can retry.

## Recording outcomes

`record_outcome(skill_id, success)` bumps the success or failure counter on a specific
version and stamps the time. A version's procedure is immutable; only its reliability
stats move. Outcomes are addressed by the skill's stable id, so a deprecated version can
still report a late outcome.

## Recording failures (bad patterns)

A failure worth remembering is more than a counter tick — it is a *failure mode*. `record_failure(skill_id, description)` is the companion to `record_outcome` for those: in
one atomic commit it bumps the skill's failure counter *and* stores a `BadPattern` — a
negative procedural memory describing what went wrong — linked to the skill by a
`HAS_FAILURE` edge. The description is embedded so the failure can later be matched
against new problems; if the embedder is down, the call fails closed rather than store a
pattern that could never resurface.

Bad patterns are negative procedural memory: immutable once written, specific to the
skill they were observed against, and surfaced with that skill so a known failure is
visible *before* the skill is reused.

## Retrieving skills

`retrieve_skills(problem, k)` is a dedicated procedural-recall path, separate from the
episodic and fact recall bundle, because skill selection ranks on a different axis:
problem match re-weighted by proven reliability.

1. **Two signals.** The problem text is embedded and run against the skill problem
   embeddings (vector), and run against the descriptions (BM25). The two are fused by
   reciprocal rank fusion — they are not score-comparable, so they combine by rank.
2. **Reliability weight.** Each candidate's problem-match score is multiplied by the
   Beta-posterior mean of its success rate, `(α₀ + s) / (α₀ + β₀ + s + f)`. With the
   default weak `Beta(1, 1)` prior a fresh, unproven skill scores a neutral `0.5` —
   neither boosted nor buried — a `1/0` skill scores `2/3` rather than an over-trusted
   `1.0`, and the weight moves toward the empirical rate as outcomes accumulate. This is
   the same Beta model the trust scoring uses, so reliability and trust stay on one
   footing.
3. **Bad-pattern penalty.** Each candidate's score is then multiplied by
   `1 / (1 + weight · count)`, where `count` is how many of the skill's recorded failure
   modes look like the current problem (their embedding's cosine similarity to the query
   clears a threshold). A skill with a known failure mode that matches what you are about
   to ask sinks below an equally-matched clean one — but a failure mode unrelated to this
   problem does not hold the skill back.
4. **Active only.** Deprecated and soft-forgotten (expired) versions are history and
   never surface.
5. **Degrade, don't fail.** If the embedder is down at query time, retrieval falls back
   to the description's BM25 index — the lexical recall floor — instead of returning
   nothing. The bad-pattern penalty is skipped (no query vector to compare against), but
   the patterns still surface so the failure modes stay visible.

The result is a list of `RankedSkill`, each carrying the skill, the score split into its
factors (`similarity` and `reliability`) so a caller can see why a skill surfaced, and
the skill's failure modes (`bad_patterns`) ordered by how relevant each is to the query.
Over-fetching (a configurable multiple of `k`) before the re-ranking lets a proven,
slightly-less-similar skill rise above an unproven exact match. Ties break by skill id,
so the order is reproducible.

## API surface

The contract lives in `aionforge_domain::contracts::ProceduralMemory`:

```rust
fn save_skill(&self, skill: Skill) -> impl Future<Output = Result<Id, Self::Error>> + Send;
fn record_outcome(&self, skill_id: Id, success: bool) -> impl Future<Output = Result<(), Self::Error>> + Send;
fn record_failure(&self, skill_id: Id, description: String) -> impl Future<Output = Result<Id, Self::Error>> + Send;
fn retrieve_skills(&self, problem: String, k: usize) -> impl Future<Output = Result<Vec<RankedSkill>, Self::Error>> + Send;
```

`aionforge-procedural` implements it with `ProceduralMemoryService`, which is generic
over the embedder seam (it names the embedding contract, not a concrete client) and
takes an injected clock so stored times are deterministic and never read from an ambient
source. `ProceduralConfig` exposes the retrieval and reliability knobs — the RRF
constant, the per-signal weights, the candidate over-fetch multiplier, the Beta prior,
and the bad-pattern penalty weight and relevance threshold — all range-checked at
construction.

## Where it sits

Procedural memory is a layer-2 subsystem. It owns the policy — version assignment,
deprecation, audit construction, change detection, and the reliability-weighted
ranking — over the layer-0 store's versioned-skill surface, which provides the atomic
write primitives and the indexed reads. Only the store names the underlying graph
engine; this layer speaks domain types.
