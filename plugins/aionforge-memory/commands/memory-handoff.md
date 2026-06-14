---
description: Capture a durable Aionforge Memory handoff for the current Claude Code work
argument-hint: Optional context to include
---

# Aionforge Memory Handoff

Context: $ARGUMENTS

Create a handoff that future agents can use without replaying the whole session.

## Procedure

1. Check current evidence: branch, recent commits, PR number, CI state, changed files, validation commands, and remaining work.
2. Search memory if prior handoffs, release state, or superseded decisions may matter.
3. Capture focused records rather than one vague summary:
   - Current status and what changed.
   - Decisions made and why.
   - Validation results and commands run.
   - Known failures, blockers, or caveats.
   - Exact next steps.
4. Leave the remaining work as **work items**, not just prose: `work_create` (or `work_advance` an existing one) so the next agent can `work_query` the backlog instead of replaying the session. Tasks are work items, not memory episodes.
5. Use `supersedes` when replacing an older handoff or corrected fact.
6. Report the new memory ids when audit, forget, restore, or supersession may matter.

## Guardrails

- Do not capture secrets, credentials, private keys, raw tokens, or private log dumps.
- Mark uncertainty clearly; do not capture guesses as facts.
- User direction wins if they ask to avoid memory or limit what gets stored.
