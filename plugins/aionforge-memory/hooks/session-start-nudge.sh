#!/usr/bin/env bash
# Aionforge Memory — SessionStart nudge hook.
#
# Re-seeds the memory cadence and vocabulary into a fresh model context. Wired in
# hooks/hooks.json to fire on SessionStart with matcher "startup|resume|compact".
#
# Why SessionStart and not PreCompact: PreCompact is blocking-only (decision/reason)
# and CANNOT inject context, so it cannot deliver a reminder. SessionStart supports
# additionalContext and its "compact" source fires AFTER a compaction completes — so
# this runs exactly when the prior working context was just discarded, which is the
# moment the cadence most needs restating. (Verified against the Claude Code hooks
# reference; see plugins/aionforge-memory/NUDGE.md for the canonical guidance.)
#
# Emits a single JSON object on stdout. Keep the reminder text ASCII with no double
# quotes or backslashes so it forms a valid JSON string without an escaper dependency.
set -euo pipefail

# Drain the event JSON on stdin so the producer's pipe closes cleanly. The reminder
# is the same regardless of source (the matcher already gates which sources fire),
# so the payload is intentionally unused.
cat >/dev/null 2>&1 || true

CONTEXT="Aionforge Memory is this project's durable memory; keep it in the loop. Recall before substantial work (and recall open work with work_query / work_tree). Capture durable facts (decisions, fixes, validation, handoffs) the moment they land, not batched to the end. Track tasks, blockers, and TODOs as work items via work_create then work_advance then work_link; they persist and are status-tracked. Durable facts go to capture and become decaying episodes; there is no note to store directly (consolidate derives those). Never store secrets. User direction overrides memory."

printf '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"%s"}}\n' "$CONTEXT"
