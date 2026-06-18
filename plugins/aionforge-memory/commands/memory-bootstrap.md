---
description: One-time setup of a foundational Aionforge Memory substrate for a fresh project
argument-hint: Optional project name or context
---

# Aionforge Memory Bootstrap

Project: $ARGUMENTS

Run the `memory-bootstrap` procedure once for this project: turn an empty store into a useful substrate so the next session recalls real context instead of starting cold. It is idempotent — recall before writing and re-running updates rather than duplicates.

## Procedure

1. Confirm the user wants a bootstrap, and check `server_status` if the MCP connection is uncertain. If it is unavailable, say so and stop.
2. Resolve identity once: prefer `AIONFORGE_AGENT_ID`; otherwise use the stable agent UUID from the user or project instructions. Ask once if none exists — do not silently mint a throwaway namespace. `capture` takes the bare UUID; recall and work tools take `agent:<uuid>`.
3. Recall first (`search` for existing conventions/decisions, `work_query` for an existing backlog) so re-runs seed only the gaps; `supersedes` an existing fact instead of duplicating it.
4. Gather ground truth from the repo (README, docs, CI config, dependency manifests) and the user. Seed only verified facts — never invented ones.
5. `capture` the durable conventions (dev workflow, branch/merge model, CI gates, coding standards, review norms) and the key architecture decisions and product direction — one focused record each, `role: assistant`, higher trust (e.g. `trust: 0.9`) for curated foundational facts.
6. Query before creating the backlog: work items have no dedup or supersede, so `work_query` (filter `level: epic`, match titles in the results) for an existing epic first. Create the `epic` only if absent, then add the missing initial `task` children via `parent_id` under it. Tasks are work items, not memory episodes.
7. Optionally seed cross-cutting facts into a `team:<name>` namespace if the user authorizes that scope; default to private otherwise.
8. Verify: `search` the seeded topics back and `work_query` the backlog to confirm they return. Summarize the categories covered and the work-item ids.

## Guardrails

- Never store secrets, credentials, private keys, or raw tokens — in memories or work items.
- Capture only verified facts from the repo or the user; do not seed speculation.
- Widen to a team scope only when the host or user explicitly grants it.
- If the user does not want a bootstrap, do not run it.
