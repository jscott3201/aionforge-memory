# Documentation

System documentation for Aionforge Memory — how the pieces work and how to use them.
This is reference and guides, not planning or changelogs.

## Subsystems

- [Procedural memory](procedural-memory.md) — skills stored as data: versioning,
  reliability, reliability-weighted retrieval, bad-pattern avoidance, and conservative
  off-by-default skill induction.
- [Completion client](completion-client.md) — the optional, off-by-default chat client:
  one provider-agnostic seam over OpenAI Chat Completions, OpenAI Responses, and Anthropic
  Messages (and any OpenAI-compatible local server), with pinned sampling and graceful degrade.

More subsystem guides land here as each one is built.
