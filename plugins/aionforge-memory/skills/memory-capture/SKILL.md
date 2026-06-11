---
name: memory-capture
description: Capture durable Aionforge Memory records for decisions, user preferences, project facts, release outcomes, validation results, and reusable failure patterns. Use when the user asks to remember something or when a session handoff should persist.
license: MIT OR Apache-2.0
compatibility: Requires an enabled Aionforge Memory MCP server.
metadata:
  aionforge-version: "0.1.0"
---

# Memory Capture

Use this skill when the user wants durable memory or when the final state of a session should be available to future agents.

## Procedure

1. Confirm there is explicit user intent to write memory. Good signals include "remember this", "save this", "capture this", "make a handoff", or a standing project rule to persist session summaries.
2. Resolve the writer identity. Prefer a known `AIONFORGE_AGENT_ID`. If none is available, ask the user for the agent UUID to use for this workflow.
3. Write one compact memory at a time. A good capture names the project, the decision or fact, the supporting evidence, and the date when that matters.
4. Call `capture` with `agent_id` set to the UUID, `role` set to `assistant` for session summaries or `event` for external project events, and `model_family` set to the active client when known.
5. If the new memory replaces an older one, pass the older memory id as `supersedes`. Treat supersession as evidence for consolidation, not as immediate deletion.
6. Report the capture receipt. Preserve the returned memory id when it may be useful later.

## What To Capture

- Decisions the user wants future agents to follow.
- Durable project facts, release status, and validation outcomes.
- Reusable workflow lessons or failure patterns.
- Handoff summaries that name branch, PR, CI state, remaining work, and important caveats.

## What To Leave Out

- Secrets, tokens, private keys, passwords, and credentials.
- Raw logs unless the exact text is needed.
- Recalled memory text copied back into memory without new verification.
- Speculation that was not checked against the current repo or the user.
