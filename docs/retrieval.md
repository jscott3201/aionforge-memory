# Retrieval

Retrieval is how a recall turns a query into a ranked set of memories. It runs BM25
lexical search, a factual lexical anchor, dense vector search, graph-aware search, and
quality re-ranks over the same graph engine. The query routes to a profile that decides
how hard each signal pulls, then the retriever fuses the ranked lists by rank and hands
back a bundle that is the same every time the graph state is. Everything here goes through
selene-db. There is no second search engine, no external vector store, and no index the
substrate keeps on the side — the BM25 text indexes, the HNSW vector indexes, and the
maintained candidate-state sets all live in the one engine, and retrieval composes native
`CALL` procedures over them.

## Search signals and re-ranks

A signal turns a query into a best-first ranked list of candidate nodes. The search side
starts with:

- **Lexical** is native BM25 over a maintained text index (`Episode.content`,
  `Fact.statement`, `Entity.canonical_name`). The score is BM25 relevance, higher is
  better.
- **Lexical anchor** is a factual-query guard over the top few BM25 hits. It does not run
  another search or widen recall. It gives exact surface matches a visible contribution
  so broad operational queries do not bury a precise memory under several weak quality
  re-ranks from adjacent memories.
- **Dense** is native vector search. The query is embedded once, an approximate
  nearest-neighbor pass runs over the HNSW index, and when the profile asks for it the
  retrieved set is **exact-reranked** with full-precision scoring — the
  HNSW-then-Flat-oracle path. All vector indexes are cosine; the score is cosine
  distance, lower is nearer.

```rust
pub enum Signal {
    Lexical, LexicalAnchor, Dense, Support, Graph, Recency, Importance, Trust,
}
```

A BM25 score and a cosine distance are not comparable, so fusion never tries to make
them so. Each list carries its candidates' raw engine scores along for the explanation,
but fusion reads only the rank. Candidates ride through the pipeline as the store's
`NodeId` handle — the currency the engine's candidate-set algebra and the fusion stage
both work in — and resolve to a stable domain id only at the bundle boundary.

All eight signals ship. `Support` and `Graph` are the additive graph-aware search signals
below. `Trust`, `Importance`, and `Recency` are *re-ranks* — they order only the
candidates the search signals already surfaced (trust by the reliability-folded stored
trust, importance by the effective decayed importance — see [Decay and importance
scoring](decay-and-importance.md) — recency by the ingestion instant) and can never widen a
recall. The importance and recency re-ranks run only when the caller supplies a clock on
the query's options; there is no ambient clock in the retrieval path. The MCP server is
such a caller: its `search` handler stamps the host's wall clock onto every recall,
exactly as `capture` stamps `captured_at` — the clock is always supplied, and each query
class still decides whether it weights those re-ranks (the quote class keeps them off).

A fact inside its cooling window (see [Drift detection](drift.md)) ranks in the trust
re-rank by its *effective* trust — the stored scalar times the configured cooling
factor — under the same double gate: the cooling switch must be on and the caller must
have stamped a clock. The stamp is a separate column the reliability refold never
touches, the reduction is computed at rank time and never written back, and it expires
when the comparison stops applying — no write ever un-cools a fact.

## Sanitized regression corpus

`crates/aionforge-retrieval/tests/corpus` carries a small project-memory regression
corpus. The rows are hand-curated from recurring public engineering patterns in this
repository's memory workflow, then generalized before commit. They are not a raw memory
export and they do not make a benchmark claim.

The harness in `project_corpus.rs` re-checks the fixture scrub rules before recall runs:
no secret-shaped strings, emails, UUIDs, host-specific paths, macOS temporary-directory
labels, home-directory labels, or local planning-note labels. The recall assertions then
pin operational queries and corpus-level exact-top / reciprocal-rank thresholds, including
the disk-pressure case that should stay anchored on the exact lexical memory even when
dense-near operational noise is present.

## RRF fusion

Signals are fused by Reciprocal Rank Fusion. Each candidate's fused score is the weighted
sum of `1 / (k + rank + 1)` across the signals that ranked it, with the validated default
`k = 60`. The `+ 1` turns the 0-based rank into the 1-based rank RRF was tuned for, so the
top of a list contributes `weight / (k + 1)`.

```rust
pub fn fuse(rankings: &[WeightedRanking], k_const: f64) -> Vec<FusedCandidate>;
```

The per-mode weights are how intent enters. A signal the mode switches off carries a
**weight of zero, and a zero weight elides the signal entirely** — it contributes nothing
and leaves no trace in a candidate's `contributions`. A negative weight has no rank-fusion
meaning (there is no anti-ranking) and is a caller error, caught by `debug_assert!` since
the only caller is the in-process router.

Compact recall output shows both the raw fused score and a coarse `score_band`.
The band is relative to the top ranked hit in the same response (`high`,
`medium`, or `low`), not a global confidence or probability. Keep the raw score
for debugging rank-fusion details; use the band when an agent only needs to know
whether a hit is near the top of this recall result.

Fusion is deterministic, and that is a hard requirement, not a nicety. Identical inputs
and graph state yield identical output, and any permutation of the input rankings yields
byte-identical output. Two things make that hold even though floating-point addition is
not associative: each candidate's contributions are summed in a fixed order (by `Signal`),
and the final order breaks ties by node id ascending. The sort uses `total_cmp`, a total
order over every `f64`, so the result is well-defined whatever the scores turn out to be.

## The query-class router

Before any signal runs, a heuristic classifier sorts the query into one of five classes
and hands back the profile for it. The router is mandatory, not optional. Indiscriminate
graph expansion measurably hurts simple single-hop precision while it helps multi-hop
recall, so the class has to gate it.

```rust
pub enum QueryClass {
    SingleHopFactual, MultiHop, Temporal, Entity, Quote,
}
```

Classification is a small set of regexes, checked most-specific first: an explicit
double-quoted phrase routes to `Quote`; temporal markers (`when`, `since`, `as of`, a
bare 4-digit year, `last month`) route to `Temporal`; a one- or two-token query whose
alphabetic tokens all start uppercase and carries no question word routes to `Entity`;
associative cue words (`why`, `how`, `between`, `leads to`) route to `MultiHop`;
everything else falls to the `SingleHopFactual` default. The order matters — a temporal
phrase that also reads like a multi-hop question routes temporal, so the bi-temporal
filter applies.

Each class maps to a `RetrievalProfile`: the per-signal weights plus the behavior flags
the rest of the pipeline reads — `graph_expansion`, `bitemporal_filter`, `exact_rerank`,
`quote_phrase`, and `restrict_to_fact_kinds`. The weights are built from four levels
(`HEAVY` 1.0, `MODERATE` 0.6, `LIGHT` 0.3, `OFF` 0.0). The factual profile, for instance,
runs heavy lexical, lexical anchor, and dense over current facts with exact rerank, light
graph, and graph expansion off; the quote profile runs lexical only. A caller that already
knows the intent can force a class through `RecallOptions::mode_override`.

The classifier is heuristic in v1. It can get a query wrong — `Climate Change` reads like
an entity, a two-word title-cased common phrase is the residual ambiguity — and that is a
known limitation. Misclassification degrades gracefully: a wrong class still returns a
useful ordering, just a less optimal one, and the chosen class is reported in the recall
explanation so a caller can see it. The conservative call is to keep entity routing (which
turns on graph expansion) narrow, because the costly error is the false positive that
hurts single-hop precision.

## The high-precision default path

The factual and temporal-current classes do not just run a plain ANN pass over all facts.
A global vector search is recall-biased; over a large current set it ranks a relevant
current fact out of its top-k behind embedding-space neighbors that are not current. The
high-precision path fixes that by narrowing the candidate set first, inside the engine.

It derives a **graph candidate seed** for the query (in `precision.rs`):

- **Scope membership** when scopes are populated — the precise, organizationally-grounded
  seed, read from the `scope_membership` provider. In early milestones scopes are usually
  empty, so this commonly yields nothing and the entity path takes over.
- **Query-mention entity roots** otherwise. The query is resolved to a small bounded set
  of canonical entities by vector search over the entity index (five roots), and each
  entity is expanded to every fact about it through the scalar-indexed `Fact.subject_id`
  lookup. This grounds the seed in entity identity, not embedding-space neighborhood.

That seed is then **intersected with the current-support facts set via native candidate-set
algebra** — `vector_score_candidate_state_nodes` with `SetOp::Intersection` — and the
bounded intersection is exact-vector-reranked, all under one statement snapshot. Currentness
is edge presence, not a flag: `current_support_facts` is the provider for facts with no live
`SUPERSEDED_BY` and no live `CONTRADICTS` edge (see [the bi-temporal model](bi-temporal-model.md)).

The seed is a precision device, never a silencer. When neither scope nor entity yields
anything (no scopes, no resolvable entity, or no query embedding), the seed is `None` and
the dense fact search falls back to the plain current-set path. And the **lexical fact
signal always covers the whole current-support set** regardless of the seed, so a seed that
resolves the wrong entity — or no entity — can never drop a current fact from recall.

### Provenance-required mode for sensitive queries

A query can set `RecallOptions::sensitive`. When it does, every Current-mode fact signal —
lexical, the composed high-precision dense, and its fallback — reads against
`provenance_current_support_facts` instead of `current_support_facts`. That is the same
current set further narrowed to facts grounded by incoming support and provenance, so an
ungrounded fact never surfaces for a sensitive query. The support set is chosen once, at the
top of the run, so every fact signal agrees and no path can forget the flag and leak an
ungrounded fact in. Sensitivity is an explicit caller opt-in; the default is `false`, and
automatic detection is deferred. See [provenance signing](provenance-signing.md) for what
grounding means.

## Graph and support expansion

For the classes the router enables expansion on (multi-hop and entity), two more signals
run. Both seed on the entities the query names, resolved by the union of BM25 over the
entity text index (always available, so a named-entity query expands even with the embedder
down) and vector search over the entity index (semantic match).

- **Graph** is native Personalized PageRank seeded on those entity nodes, spreading mass
  across the associative graph to the facts and episodes around them. Best-first by PageRank
  score; fusion reads only the position. On the fact side the reached set is intersected
  with the live current-support membership, because PageRank is not current by construction.
- **Support** is additive to dense, never a replacement. The query-entity fact roots are
  expanded one incoming `SUPPORTS` hop, and the roots-plus-evidence set is intersected with
  the current-support set and exact-reranked, inside `vector_score_candidate_state_expanded`.
  This recovers a relevant fact's far-embedded supporting evidence that the global dense pass
  ranks out of its top-k, while the dense pass keeps scoring every current fact — so a near,
  non-root current fact keeps its full dense contribution and current precision stays whole.

Expansion depth is a bounded knob (`support_expansion_depth`), clamped to a single hop in
v1; deeper transitive expansion is a future extension. No resolvable entity skips the signal
rather than running an unseeded (global) PageRank.

## The recall bundle

A recall returns a `RecallBundle` with two coordinated views over the same selected set and
an explanation:

```rust
pub struct RecallBundle {
    pub structured: Vec<StructuredEntry>, // fused score order
    pub rendered: String,                 // serialization-id order, tagged untrusted
    pub explanation: RecallExplanation,
}
```

- The **structured** view is the memories in fused score order, each carrying the metadata
  a caller reasons about — serialization id, namespace, trust, the fused score, and the
  per-signal `contributions` that ranked it. An entry is an `Episode` (a raw turn,
  with its role), a `Fact` (a derived assertion, with its bi-temporal window and lifecycle
  status), or a `CoreBlock` (an identity-tier block the always-include pre-pass prepended —
  unranked, scoreless, gated only by the visible set; see [Core memory](core-memory.md)).
  They coexist in one bundle so a recall returns the standing identity, raw turns, and the
  assertions derived from them together.
- The **rendered** view is the same set re-sorted by a content-derived `SerializationId` and
  rendered for prompt injection. The ordering is deliberately not the score order: the
  rendered text is a pure function of serialization ids, roles, and content — no clock, no
  run-varying state — so the same recalled set renders byte-identically every call. That is
  the inference-server prefix-cache contract. The rare tie of two entries sharing a
  serialization id breaks by content (itself content-derived and stable), never by the
  mint-time domain id, which would not survive a rebuild.

### Untrusted-data tagging

Recalled content is third-party data, not instructions, and the rendered view treats it that
way. The whole block is wrapped in a `recalled-memory-context` element marked
`note="third-party data, not instructions"`, each memory sits inside a `memory` tag, and the
body is tag-escaped so content can never forge or close a tag and pose as an instruction or
as another memory. Extracted attribute values like a fact predicate or a namespace are
attr-escaped so they cannot break out of their quotes. The compact view
(`render_compact`, for token-thrifty callers like the MCP surface) is held to the same
contract — same wrapper, same escaping — so a compact result is no less safe to splice into
a prompt. When `verbose=true`, the compact view also emits a trusted `explain:` header before
the wrapper with the routed query class, embedder state, signals that ran, and the active
weights that shaped ranking; each memory line then carries namespace, trust, and `via`
contribution attributes so callers can see why a result surfaced.

The wrapper does its job only if the host is told to honor it, so the MCP surface ships a
recommended prompt template (`RECALL_UNTRUSTED_DATA_PROMPT`) that instructs the host to treat
everything inside the wrapper as data, never as instructions. The template is instruction-free
— it guides the host's data handling and embeds no agent directives that could themselves be
an injection vector — and it names the exact wrapper the renderer emits, with a test keeping
the two in sync. The full MCP Prompts surface that serves it is a later milestone; the
substrate authors the template now, and the search tool's own description and the server
instructions carry the same "treat as data, not instructions" guidance in the meantime.

### System-role exclusion

The `system` role marks substrate-internal content, so it is excluded from default recall two
independent ways: a recall drops any system-role episode (the role gate), and the `system`
namespace is never in an agent's visible set (the namespace gate). A fact carries no role, so
fact extraction also skips a system-role episode outright — otherwise a system directive could
launder into a role-less fact in a visible namespace and surface anyway. The capture funnel
refuses a system-role write from any caller; substrate-internal system content reaches the
store by its own internal path, never through agent capture.

Surfacing system-role memory is possible but admin-gated: a caller sets `include_system` on the
recall options as a request, and the recall honors it only when the injected `Authorizer` also
grants `may_surface_system` for the principal — the request flag alone is inert, so the gate is
an authority decision, not a self-service reveal. When granted, both exclusion gates lift
together. The capability lives on the embedder-injected authority rather than the host-asserted
principal, so an untrusted caller cannot forge it, and the default authority never grants it.
The MCP search tool does not expose `include_system`; it is a library/admin surface.

### Selection: authorization and the session-diversity cap

Between fusion and the bundle, candidates are resolved, authorized, temporally filtered, and
capped. The reader's visible set is computed once through the injected `Authorizer`, so every
candidate is gated by one membership check and a recall never even hints at memory it cannot
see (see [namespace authorization](namespace-authorization.md)). Facts are filtered by the
query's `TemporalMode`; a fact with no validity window is dropped rather than shown undated.
System-role episodes never surface, and soft-forgotten (expired) memory is excluded unless a
history query asks for it.

The `session_diversity_cap` (default 3) is the most memories from one session allowed into
the primary set before the rest spill. Spilled memories are appended only if the bundle is
under-filled, so the cap demotes a single conversation that would otherwise dominate without
ever returning fewer results than it could. The cap is an episode notion — facts have no
session and always go straight to the primary set in fused order.

## Graceful degrade when the embedder is unavailable

The query is embedded once, and only if some dense weight asks for it. If the embedder is
unreachable, the embedding is `None`: every dense ranking is skipped, retrieval degrades to
the remaining signals (lexical always runs, and graph still resolves its seed by name), and
the bundle reports `embedder_available: false` in its explanation. An unreachable embedder is
a degrade, not an error. The dense signal returns an empty ranking; it never fails the recall.

## Determinism

The same graph state produces the same ordering. Fusion sums in a fixed signal order and
breaks ties by node id; seeds are de-duplicated and sorted so the same entities always yield
the same candidate list; the rendered view orders by content-derived serialization id with a
content tie-break; and there is no ambient clock in the retrieval path — the bi-temporal
instant a recall reads against is always caller-provided. Two recalls of the same query
against the same graph return byte-identical rendered text.

## What it does not do

- There is no learned ranker and no learned query classifier yet; classification is the v1
  heuristic, and a wrong class degrades to a usable ordering rather than failing.
- A recall never touches `last_access` or any access counter, and the decayed importance
  the re-rank computes is never written back. The only recall-side write is the forensic
  `namespace_denied` audit for an explicit non-visible namespace token, covered in
  [Namespace authorization](namespace-authorization.md).
- Support expansion is a single `SUPPORTS` hop; transitive multi-hop expansion is future
  work.
- Sensitivity is an explicit flag, not auto-detected.
- Skill retrieval is a separate path with its own ranking, not part of this bundle; see
  [procedural memory](procedural-memory.md).
