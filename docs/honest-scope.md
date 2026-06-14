# Honest scope and deferred work

Aionforge Memory is an exemplar-based memory substrate. It stores episodes,
facts, notes, skills, bad patterns, identity blocks, and audit events; retrieves
them with native lexical, vector, graph, temporal, trust, and recency signals;
and renders recall as untrusted data for a host model. It is not a training
system, not a fine-tuning loop, and not an autonomous model router.

This page is the v1 scope boundary. It is intentionally conservative: no public
claim should exceed what the current code and gates prove.

## In scope for v1

- Rust library API through the `aionforge` crate and `Memory` facade.
- MCP Tools, Resources, and Prompts over stdio and Streamable HTTP.
- Single binary operator path: `doctor`, `recover`, and `serve`.
- WAL-backed persistence over selene-db, with native BM25, vector, graph, and
  provider-state rebuilds.
- Capture, consolidation, retrieval, forgetting, erasure, core memory, trust,
  promotion, audit signing, drift detection, and red-team probes as documented in
  the subsystem guides.
- Optional embeddings from one configured OpenAI-compatible endpoint.
- Deterministic, rule-based consolidation only: fact extraction, summarization,
  skill induction, and off-cursor note link evolution. No chat/completion model is
  called anywhere in the substrate.
- Alpine Docker runtime image, tag-driven release artifact publishing, and
  GHCR images for `linux/amd64` and `linux/arm64`.

## Out of scope for v1

The table below lists deferred work and the current safe posture.

| Area | v1 posture | Deferred work |
|------|------------|---------------|
| Benchmarks | M7 is deferred, so docs make no benchmark-backed latency, quality, or cost claims. | M7 benchmark suite. |
| LLM-backed consolidation | Not shipped. Consolidation is deterministic and rule-based only (rule summarization/extraction/induction plus deterministic off-cursor link evolution); the substrate calls no chat/completion model. The cross-family consolidation guard remains as standing substrate policy over the `LinkEvolver` inference seam. | A benchmark-backed quality bar before any inference-backed consolidation rule could be reintroduced. |
| Cost-first routing | Not supported. A deployment declares one embedding provider and no chat provider. | Any multi-provider routing needs a separate verifiability design. |
| Experiential hand-off | Not shipped. Aionforge stores exemplars and derived memory, not a transferable live experience/state interface. | A future hand-off protocol and evaluation plan. |
| Snapshot lifecycle | Recovery replays the WAL and rebuilds derived indexes/providers; the store does not yet drive scheduled snapshot publication or WAL truncation. | Snapshot publication, WAL truncation, and backup automation. |
| Erasure residency | Hard purge removes live graph reachability, but purged values can remain in the WAL until snapshot publication exists. | Compaction-backed residency guarantees. |
| Pi support | Intentionally deferred. | Read Pi's packaging/extension model and design a native integration. |
| Semantic contradiction cooling | Drift cooling uses deterministic vector proximity to buy the detector time. | Any LLM contradiction classifier must stay off-cursor and opt-in. |
| OAuth verifier | The crate has protected-resource metadata helpers and explicit MCP `principal` parameters for a verified host to pass identity, but the built-in HTTP server does not validate OAuth tokens or infer identity from transport state. | Full remote deployments must add an upstream OAuth verifier and map verified claims into the explicit principal object. |
| External source references | No node carries a file/URI/path field. Provenance is sibling-node ids plus model/transport identity only, and the event body is stored inline in `Episode.content` (see [data-model.md](data-model.md) §4). | A first-class source-link field and indexable external references. |
| Capture content sizing | No substrate-level min/max/quota on `Episode.content` — it is an unbounded string. The only byte bound is the 1 MiB Streamable-HTTP request-body limit, shared across a `batch_capture` of up to 64 items, and it does not apply to the in-process library path. | Optional ingress size/quota policy. |
| Entity over-segmentation | Conservative, namespace-confined, split-leaning entity resolution can fragment one real-world thing into duplicate `Entity` nodes, diluting the entity-seed retrieval path; there is no merge-repair tool (see [data-model.md](data-model.md) §7). | A reconciliation/merge-repair pass with safety review. |

## Determinism boundary

Canonical capture and consolidation are deterministic for the same inputs and
graph state. Retrieval ordering is deterministic for the same query and state.
The substrate does not read ambient time in canonical ranking decisions; callers
provide clocks where time-dependent behavior is needed.

Off-cursor link evolution runs outside that canonical boundary. Its `RELATES_TO`
edges are derived, linked back to source notes, off by default, and deterministic
for the same notes and rule version. They are non-canonical: they can enrich the
note graph, but they never change byte-identical recall of the canonical state.

## Security boundary

Security claims stop at the memory substrate boundary. Aionforge can refuse
unauthorized reads and writes, verify signed writes, sign audit rows, keep system
memory out of default recall, and render recalled content as untrusted data. The
host still owns model behavior, client approval decisions, network perimeter,
OAuth validation, secret custody, and backups.

## Release posture

The release gate asserts the binding acceptance criteria that exist in code:
workspace tests, clippy, doctests, rustdoc link checks, dependency audit, license
attribution, repository policy gates, Docker build, and explicit M6 red-team
probes. The `0.2.1` release publishes GitHub Release artifacts, Linux and macOS
native binaries, and GHCR runtime images for `linux/amd64` and `linux/arm64`.
crates.io publishing is deferred until the selene-db 1.x crates are available
from crates.io. Tagged releases are cut only after human sign-off.
