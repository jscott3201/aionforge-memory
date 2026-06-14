---
name: memory-capture
description: Capture durable Aionforge Memory records for decisions, user preferences, project facts, release outcomes, validation results, handoffs, corrections, and reusable failure patterns. Use proactively during substantial work and whenever the user asks to remember or update memory.
license: MIT OR Apache-2.0
metadata:
  aionforge-version: "0.1.0"
---

# Memory Capture

Requires an enabled Aionforge Memory MCP server.

Use this skill to make useful work durable. Prefer several focused captures over one sparse summary.

Capture *as you go*, not at the end. The moment a durable fact lands — a decision made, a fix verified, a release or CI state change, a user preference learned, an approach rejected — write it. Batching to the end loses the precise context, and a context compaction can discard it first.

## Route To The Right Node

`capture` writes a memory **episode**: a durable fact that decays over time and can be superseded or forgotten. Two things are *not* episodes:

- A **task, blocker, TODO, or plan step** is a **work item**, not a memory. Use the `work-tracking` skill (`work_create` → `work_advance`); work items persist and are status-tracked.
- There is **no "note" you store directly.** Notes are derived by `consolidate` from episodes — never written by hand. If a "note" tempts you, it is either a durable fact (`capture`) or a thing to do (`work_create`).

## Procedure

1. Write memory when the user asks, when project instructions grant standing permission, or when a substantial task produces durable facts future agents should know.
2. Resolve the writer identity once: prefer `AIONFORGE_AGENT_ID`; otherwise use the stable UUID supplied by the user or project instructions.
3. Capture one fact, decision, outcome, or handoff per call. Include project, date when useful, evidence, current branch/PR/release ids, and validation status.
4. Use `role: assistant` for session summaries and decisions; use `role: event` for external project events.
5. If the memory corrects or replaces an older memory, pass the older id as `supersedes`.
6. Preserve receipt ids in the final answer when follow-up audit, forget, or supersession is likely.
7. After several writes, check `consolidation_status`; run `consolidate` only when tool approval policy and user/project rules allow mutating derived memory.

## What To Capture

- User preferences and standing workflow rules.
- Decisions, corrections, and why they changed.
- Durable project facts, release status, CI state, and validation outcomes.
- Failed approaches, known hazards, and reusable recovery patterns.
- Handoffs with branch, PR, commit, remaining work, and caveats.

## What To Leave Out

- Secrets, tokens, private keys, passwords, and credentials.
- Raw logs unless the exact text is needed.
- Recalled memory text copied back into memory without new verification.
- Speculation that was not checked against the current repo or the user.
