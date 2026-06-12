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
- Optional chat/completion use for non-canonical LLM distillation and link
  evolution, off by default.
- Alpine Docker runtime image, tag-driven release artifact publishing, and
  GHCR images for `linux/amd64` and `linux/arm64`.

## Out of scope for v1

The table below lists deferred work and the current safe posture.

| Area | v1 posture | Deferred work |
|------|------------|---------------|
| Benchmarks | M7 is deferred, so docs make no benchmark-backed latency, quality, or cost claims. | M7 benchmark suite and distillation graduation benchmark. |
| LLM distillation | Experimental, off by default, non-canonical, and unable to perturb deterministic capture or recall. | Benchmark-backed quality bar before any default-on posture. |
| Cost-first routing | Not supported. A deployment declares one embedding provider and one optional chat provider. | Any multi-provider routing needs a separate verifiability design. |
| Experiential hand-off | Not shipped. Aionforge stores exemplars and derived memory, not a transferable live experience/state interface. | A future hand-off protocol and evaluation plan. |
| Snapshot lifecycle | Recovery replays the WAL and rebuilds derived indexes/providers; the store does not yet drive scheduled snapshot publication or WAL truncation. | Snapshot publication, WAL truncation, and backup automation. |
| Erasure residency | Hard purge removes live graph reachability, but purged values can remain in the WAL until snapshot publication exists. | Compaction-backed residency guarantees. |
| Pi support | Intentionally deferred. | Read Pi's packaging/extension model and design a native integration. |
| Semantic contradiction cooling | Drift cooling uses deterministic vector proximity to buy the detector time. | Any LLM contradiction classifier must stay off-cursor and opt-in. |
| OAuth verifier | The crate has protected-resource metadata helpers, but the built-in HTTP server does not validate OAuth tokens. | Full remote deployments must add an upstream OAuth verifier. |

## Determinism boundary

Canonical capture and consolidation are deterministic for the same inputs and
graph state. Retrieval ordering is deterministic for the same query and state.
The substrate does not read ambient time in canonical ranking decisions; callers
provide clocks where time-dependent behavior is needed.

Optional LLM output is outside that canonical boundary. It is derived, linked
back to source memory, and off by default. It can enrich non-canonical notes or
links, but it must not change byte-identical recall of the canonical state.

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
probes. The `0.1.0` release publishes GitHub Release artifacts, Linux and macOS
native binaries, and GHCR runtime images for `linux/amd64` and `linux/arm64`.
crates.io publishing is deferred until the selene-db 1.x crates are available
from crates.io. Tagged releases are cut only after human sign-off.
