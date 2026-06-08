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
- [LLM distillation](distillation.md) — the optional, off-by-default layer that condenses facts
  into notes with a chat model, run off the consolidation cursor so it can never perturb the
  byte-deterministic canonical path; guarded against lossy output and degrading to the rule tier.
- [Note link evolution](link-evolution.md) — the optional, off-by-default layer that draws and
  revises bi-temporal `RELATES_TO` edges between notes with a chat model, off the cursor and behind
  a closed relationship vocabulary, a confidence floor, and per-run cascade caps; degrades to a
  deterministic rule tier.

More subsystem guides land here as each one is built.
