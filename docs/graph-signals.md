# Graph signals

Two of Aionforge Memory's retrieval signals come from the graph rather than from a text or vector index alone. Both turn the associative structure between memories into recall: the entities a query names, the facts about them, and the evidence that supports those facts. They exist for one reason — to recover the memory a single-hop search misses without dragging down the precision a single-hop search is good at. Both run natively in selene-db, so the graph is walked where it lives instead of pulled into Rust and traversed there.

The router decides whether either runs. Graph work is not free, and indiscriminate graph expansion measurably hurts simple single-hop precision while it helps multi-hop recall. So the class gates it: the two associative signals run only for the classes that benefit (`MultiHop` and `Entity`), and are suppressed for `SingleHopFactual`, `Temporal`, and `Quote`. See [the router](#routing) below for the gate.

## The two signals

There are two, and they are not the same mechanism wearing two hats:

- **Associative PageRank** (`Signal::Graph`) — seeded Personalized PageRank over an associative projection. It spreads mass from the entities a query names out to the facts and episodes around them, and ranks each kind by how near it lands. This is the associative *prior*: it reaches memory that has no lexical or vector overlap with the query at all, just a path through shared entities.
- **Graph-expanded support scoring** (`Signal::Support`) — additive vector scoring over a set of fact roots expanded one `SUPPORTS` hop. It recovers a relevant fact's supporting evidence whose embedding sits too far from the query for the global dense pass to rank it. This is a precision-preserving add-on to the dense signal, not a replacement for it.

Both feed reciprocal-rank fusion. Fusion reads only a candidate's *position* in each list, not the raw engine score, so a PageRank score (higher is nearer) and a cosine distance (lower is nearer) never have to be made comparable. They just contribute their ranks.

## Associative PageRank

The signal seeds Personalized PageRank on the entities a query mentions and reads back the facts and episodes that sit closest to them in the associative graph. The projection it runs over spans three node labels — `Entity`, `Fact`, `Episode` — and three edge types — `MENTIONS`, `ABOUT`, `SUPPORTS` (`Episode -MENTIONS-> Entity`, `Fact -ABOUT-> Entity`, and `Fact|Episode -SUPPORTS-> Fact`). That is the closed set of records that reference an entity, plus the support chain that links facts to each other.

The projection runs **undirected**. That is a deliberate choice. The associative schema points records *at* entities, so under natural directed PageRank an entity is a sink: personalized mass seeded on a query-mention entity cannot reach the facts and episodes that reference it. Running with selene's `undirected` orientation spreads the seed's mass across each incident edge regardless of stored direction — entity ↔ fact ↔ fact — which is exactly the associative prior this signal is for.

```rust
// crate::store::Store
pub fn personalized_pagerank(
    &self,
    kind: SearchKind,
    seeds: &[NodeId],
    k: usize,
) -> Result<Vec<SearchHit>, StoreError>
```

`seeds` are the entity nodes a query names; the returned hits carry `kind`'s label, best-first by PageRank score. PageRank runs with the conventional `0.85` damping, a 30-iteration cap, and a `1e-6` convergence tolerance — it converges well within that on a memory-sized graph. Each seed gets the same restart weight; selene normalizes the set.

An empty seed list yields an empty ranking. A graph signal needs a personalization root, and a uniform (unseeded) PageRank is not the associative prior this signal exists for — so the retriever skips the signal rather than running a global PageRank when no entity resolves.

### One session, an ephemeral projection

A selene graph projection lives in the *session*, not the graph, so the projection build and the PageRank over it run in a single session (`execute_session`, not two `execute` calls). The projection is built per call and dies with the session. That is the whole of the "generation-aware projection caching" story honestly stated: there is no cross-call projection cache to invalidate as the graph advances, because nothing outlives the call that built it. The projection cannot drift from the graph it was built over, because it never survives long enough to.

### Bounded by construction

`algo.pagerank` takes a trailing `result_label` and `limit`. The procedure filters the scored nodes to the requested label and truncates to the top `k` by score *inside the engine*, before any rows cross back to Rust. A `WHERE score > 0.0` yield-filter then drops any node outside the seed's connected component (it received zero personalized mass). So the yielded rows already *are* the ranking — the top `k` nodes of one `SearchKind`, best-first — with no full-graph transfer, no label scan, and no Rust-side sort or truncate.

### How the retriever consumes it

For a graph-expansion class with a non-zero `graph` weight, the retriever resolves the query's seed entities and runs PageRank once per kind. Seeds come from two resolvers, unioned: BM25 over the entity text index (matching canonical names, always available) and, when the query was embedded, vector search over the entity index. The lexical half keeps graph expansion alive when the embedder is down — a bare-entity query still resolves its entity by name — while the dense half catches entities named by meaning rather than surface form.

The episode side rides PageRank's reach unscoped. The fact side is different. PageRank spreads associatively across the whole graph, so its hits are not current by construction the way the lexical and dense fact searches are. In `Current` temporal mode the fact hits are intersected with the live `current_support_facts` membership before fusion, so graph expansion can never surface a fact the support provider excludes. No current fact is *lost* to that filter, because the lexical fact signal already covers the whole support set — graph expansion only ever *adds* associative weight to facts the other signals also reach.

## Graph-expanded support scoring

The second signal is additive vector scoring over a `SUPPORTS`-expanded set of roots. The roots are the query-entity fact roots; they are expanded one *incoming* `SUPPORTS` hop to gather the evidence that supports them, and the resulting `roots ∪ evidence` set is vector-scored against the query, composed with the current-support set, all under one statement snapshot.

```rust
// crate::store::Store
pub fn vector_score_state_expanded(
    &self,
    kind: SearchKind,
    query: &Embedding,
    set: CandidateSet,
    roots: &[NodeId],
    edge: ExpandEdge,        // ExpandEdge::Supports
    direction: ExpandDirection, // ExpandDirection::Incoming
    op: SetOp,               // SetOp::Intersection
    k: usize,
) -> Result<Vec<SearchHit>, StoreError>
```

With `set = current_support_facts` and `op = Intersection`, the scored set is current-scoped natively: support-derived evidence facts a plain ANN pass misses surface, while non-current facts are filtered out by the same composition. The roots are preserved by the expansion, so a query-entity fact is re-affirmed (it scores in both the dense pass and this one) rather than dropped. Empty roots yield an empty ranking — no expansion root, no global scan.

### Additive, never a replacement

This signal scores only the evidence around the query's entities. The dense signal, running alongside it, keeps scoring *every* current fact. That separation is the point. A relevant fact's supporting evidence can sit far from the query in embedding space — far enough that the global dense ANN ranks it out of its top-`k`. Support scoring gives that evidence its own rank, while the dense pass's precision over the rest of the current set is left untouched. So a near, non-root current fact keeps its full dense contribution, and current precision stays whole. You get the recovered evidence *and* the precision floor, not a trade between them.

### Bounded, tunable depth and fan-out

The expansion depth is a config knob, `RetrieverConfig::support_expansion_depth`, clamped to a hard ceiling:

```rust
const MAX_EXPANSION_DEPTH: usize = 1;
```

`0` disables support expansion (the dense pass alone stands). The default is `1`, and v1 expands exactly one `SUPPORTS` hop — the ceiling is `1`, so deeper transitive expansion is clamped out for now even when a larger value is requested; the knob already carries the requested depth for when that ceiling moves. Fan-out is the per-signal `effective_fanout`: the query's fan-out, else the configured default (`50`), never below the requested bundle size. That bounds how many roots-plus-evidence candidates the engine scores and returns.

### The gate stays disjoint from the high-precision seed

There is a separate fact-level seed — `derive_graph_seed` — that the dense fact path uses as a *precision* device (scope membership, else the entities the query names, expanded to the facts about them). The support signal's roots are the same query-entity facts that seed derives. To keep that from being computed twice or, worse, from blurring the two purposes, the gates are kept disjoint:

- The high-precision dense seed is derived only when `exact_rerank` is on and the mode is `Current` — the `SingleHopFactual` and `Temporal` classes.
- The support signal runs only when `graph_expansion` is on with a non-zero `support` weight, in `Current` temporal mode — the `MultiHop` and `Entity` classes.

Those class sets do not overlap. A single recall resolves the roots at most once, and the precision seed never doubles as associative expansion. No resolvable entity (empty roots) skips the support signal rather than running an unscoped expansion.

## Routing

The class profile decides which of these runs and how heavily. The `graph_expansion` flag is true only for `MultiHop` and `Entity`; both classes also weight the two signals:

| Class | `graph` (PageRank) | `support` | `graph_expansion` |
|---|---|---|---|
| `SingleHopFactual` | light | off | off |
| `MultiHop` | heavy | moderate | on |
| `Temporal` | off | off | off |
| `Entity` | heavy | moderate | on |
| `Quote` | off | off | off |

A small `graph` weight survives on `SingleHopFactual`, but with `graph_expansion` off the PageRank signal never actually runs for it — the flag is the real gate, the weight only shapes fusion if a signal ran. The entity heuristic that drives `Entity` routing is deliberately conservative (a one- or two-token proper-noun lookup with no question words), because the costly error is the false positive: entity routing turns graph expansion on, and that is the path that hurts single-hop precision.

## What it does not do

- **No unseeded PageRank.** With no resolvable entity there is no personalization root, and the signal is skipped. A uniform global PageRank is never run as a fallback.
- **No transitive support expansion yet.** The depth ceiling is one `SUPPORTS` hop. The knob accepts a larger value, but it is clamped to `1` until the ceiling moves.
- **No persistent projection.** The associative projection is session-scoped and ephemeral. There is no projection kept warm across recalls, and therefore no cache to invalidate when the graph changes.
- **No current-fact precision trade.** Neither signal narrows or replaces the dense pass. PageRank's fact reach is intersected with the live support set in `Current` mode, and support scoring is additive to dense — so neither can drop a current fact the other signals already cover.

## Where it sits

Both signals are composed in the retriever (`HybridRetriever`) over the layer-0 store primitives. `personalized_pagerank` and `vector_score_state_expanded` are the only places the engine's PageRank and support-expansion procedures are named; everything above them speaks `NodeId` handles and domain types. The fusion stage, the namespace check (see [namespace authorization](namespace-authorization.md)), and the temporal filter run the same regardless of which signals contributed — a graph-derived candidate is authorized, dated, and diversity-capped exactly like a lexical or dense one.
