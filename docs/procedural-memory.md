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

## Induced skills

Most skills are authored: an agent (or its host) writes them and saves them. The
substrate can also **induce** a skill during consolidation — but conservatively, and
**off by default**. Induction is the substrate noticing that an agent keeps doing the
same thing and capturing it as a reusable procedure, without ever inventing or executing
one.

Induction runs as a separate consolidation pass, so an installation that does not opt in
carries no induction footprint at all. When enabled, a single episode is induced into a
skill only when **every** conservative gate holds:

- **Off by default.** It runs only when `induction.enabled` is set.
- **A produced procedure.** Only an assistant or tool episode is a candidate — never a
  user message or a system message.
- **The agent's own namespace.** The episode must live in a private (`agent:`) namespace,
  and the induced skill is confined to exactly that namespace. This is a checked
  precondition, not just an inherited field, so team, global, and system memory never
  induce.
- **Reuse evidence.** The episode's exact content must have recurred at least a configured
  number of times (default three) within a bounded recent window in that namespace.
  Repetition is the deterministic stand-in for "this procedure is worth keeping": there is
  no executor and no recorded-outcome signal at this layer (those arrive with the optional
  model-backed distillation work), so what the substrate can see is that the agent came
  back to the same procedure again and again.
- **Enough substance.** The content must clear a small lexical floor — a minimum length
  and a minimum number of distinct words — so a repeated one-liner or an error log never
  becomes a skill.

An induced skill is transparent by construction: its body is the recurring episode's text
**verbatim** — there is no summarization or extraction step, so there is nothing for the
substrate to get subtly wrong, and an operator auditing it sees exactly the procedure the
agent repeated. It is flagged `induced`, starts with a neutral reliability prior and zero
successes (it earns its track record like any skill), is recorded with an `induce_skill`
audit, and links back to its source episode by `DERIVED_FROM`. Because the M3 inducer is a
pure rule with no embedder, an induced skill is retrieved by its description's BM25 surface
(the lexical recall floor) rather than by vector similarity.

Two safety lines are worth stating plainly. The substrate **never executes** a skill — an
induced body is inert data, and induction introduces no executor. And an induced skill is
**never auto-promoted** across a trust boundary; it stays private to the agent that
produced it. Promotion to a shared namespace is a separate, attested path (multi-agent
trust) and is out of scope here. These together are the mitigation for poisoned procedural
memory: even a deliberately-repeated bad procedure stays private, never runs, and only ever
surfaces if it actually matches a later problem.

Induction is **idempotent**. An induced skill's id is content-addressed over its namespace,
the recurring content, and the inducer's rule version, and it is written in the same atomic
transaction as the episode's consolidation flip. Re-consolidating the same episode (after a
crash, say) reconstructs the same id and writes nothing the second time. Bumping the
inducer's rule version changes the id, which cuts a fresh version under the same name and
deprecates the prior one — deprecate-never-delete, exactly like an authored skill.

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

Induction lives in the consolidation subsystem, not here: it is a separate, off-by-default
consolidation pass over the same layer-0 skill surface, built on a `SkillInducer` seam
(the deterministic `RuleInducer` in M3, a model-backed inducer in the optional distillation
layer) and tuned by an `InductionConfig`. It writes induced skills atomically with the
episode flip, so a crash commits both or neither.
