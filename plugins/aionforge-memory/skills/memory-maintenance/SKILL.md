---
name: memory-maintenance
description: Inspect, consolidate, audit, forget, or restore Aionforge Memory. Use when the user asks about memory health, backlog, provenance, stale records, corrections, deletion, restoration, or why a memory was recalled.
license: MIT OR Apache-2.0
metadata:
  aionforge-version: "0.1.0"
---

# Memory Maintenance

Requires an enabled Aionforge Memory MCP server.

Use this skill when the task is about the memory system itself.

## Procedure

1. Call `server_status` if connection state is unknown.
2. Call `consolidation_status` to inspect backlog or failed derived work.
3. Run `consolidate` only when the user/project policy permits mutating derived memory.
4. Use `audit_history` to explain provenance, capture receipts, supersession hints, lifecycle changes, and rejection reasons.
5. Use `forget` or `unforget` only when the user explicitly names the target memory id or asks for a specific lifecycle change.
6. After lifecycle changes, search again when the user needs confirmation of visible recall state.

## Defaults

- Preserve memory ids in answers.
- Prefer audit evidence over guesses.
- Treat `ERR_NOT_FOUND` as absent or unauthorized; do not infer hidden memory existence.
