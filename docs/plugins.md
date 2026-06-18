# Agent Plugin

Aionforge Memory ships a plugin package at
[`plugins/aionforge-memory`](../plugins/aionforge-memory). It bundles six Agent
Skills plus a Claude Code steward agent, commands, and a SessionStart nudge hook.

The plugin is meant to make the existing MCP service easier to use. It does not
add a second server and it does not execute stored skills from memory. The MCP
server still owns capture, recall, lifecycle tools, safety wrappers, and
approval hints.

## What It Includes

- `memory-bootstrap`: one-time, idempotent cold-start setup that seeds a
  foundational substrate for a fresh project — identity, conventions,
  architecture decisions, and a work-item backlog skeleton — so the next session
  recalls real context instead of an empty store.
- `memory-loop`: use memory through a whole task: recall first, capture useful
  state during work, and finish with a handoff.
- `memory-recall`: search durable memory before planning, coding, review,
  debugging, release, or support work.
- `memory-capture`: capture decisions, project facts, validation results, and
  session handoffs as they happen, when the user wants them persisted or project
  policy grants standing permission.
- `work-tracking`: track tasks, blockers, TODOs, and plans as durable work items
  (`work_create` → `work_advance` → `work_link`), distinct from decaying memory
  episodes.
- `memory-maintenance`: inspect backlog, audit provenance, consolidate derived
  work, forget, or restore memory.
- Claude Code agent `aionforge-memory-steward`: keeps recall, capture,
  work-tracking, and handoff in the main task loop when the plugin is enabled.
- Claude Code SessionStart hook: re-seeds the capture/work-tracking cadence into a
  fresh context after a startup, resume, or context compaction. (`PreCompact` is
  not used: it is blocking-only and cannot inject context.)
- Claude Code commands `/aionforge-memory:memory-bootstrap`,
  `/aionforge-memory:memory-session`, and `/aionforge-memory:memory-handoff`:
  explicit workflows for one-time project setup, starting a memory-backed task,
  and ending with a durable handoff.
- Cursor always-apply rule `rules/aionforge-memory.mdc` (`alwaysApply: true`):
  declared by the Cursor manifest's `rules` field. On Cursor builds that surface
  plugin-bundled rules it registers as an always-apply rule (confirm or adjust it under
  Settings > Rules); otherwise drop the same `.mdc` into your project's `.cursor/rules/`.
- Client manifests for Codex, Claude Code, and Cursor.

For OpenCode and the best-effort tier (Windsurf, Cline, Zed, Gemini CLI, Aider), the
same nudge is a short block the user adds to that editor's always-on instructions
file; [`agent-nudges.md`](agent-nudges.md) is the per-editor wiring guide. All of it
distills the canonical `plugins/aionforge-memory/NUDGE.md`.

## Identity Setup

The MCP tools require an explicit agent identity. Use one stable UUID across
sessions when you want the same private memory namespace.

Recommended local setup:

```bash
export AIONFORGE_AGENT_ID="<uuid>"
aionforge serve http --listen 127.0.0.1:3918
```

If a client cannot read `AIONFORGE_AGENT_ID`, put the UUID in that client's
standing instructions and have the skills use `agent:<uuid>` for recall and the
raw UUID for capture. Keep the local HTTP endpoint on loopback; use an external
OAuth resource-server verifier before exposing it remotely.

## Client Notes

Codex can discover the plugin from the repo-scoped
`.agents/plugins/marketplace.json`. After installation, `codex plugin list`
shows the marketplace-qualified plugin id. For the repo marketplace, the id is
`aionforge-memory@aionforge-plugins`.

The Codex plugin does not register its own MCP server. Configure the Aionforge
MCP endpoint separately as `[mcp_servers.aionforge_memory]`; that standalone MCP
entry owns tool policy and transport settings. The plugin skills declare a
dependency on that canonical server id instead of creating a second
plugin-scoped server.

Claude Code marketplace installs can discover the same package from the
repo-scoped `.claude-plugin/marketplace.json`. Direct local testing still works
without the marketplace file:

```bash
claude --plugin-dir ./plugins/aionforge-memory
```

The Claude manifest loads `skills/`, `commands/`, and `agents/`. It does not
register an MCP server; configure the Aionforge MCP endpoint separately as
`aionforge-memory` so the plugin does not collide with a user-managed server of
the same name. The plugin root `settings.json` selects
`aionforge-memory-steward` as the default main-thread agent so ordinary Claude
Code work starts with the memory loop available.

Cursor can load the package as a local plugin from
`~/.cursor/plugins/local/aionforge-memory`; it reads `.cursor-plugin/plugin.json`,
which declares both `skills/` and the bundled always-apply rule at
`rules/aionforge-memory.mdc`. On Cursor builds that surface plugin-bundled rules, that
rule registers as an always-apply rule (confirm it under Settings > Rules); if a build
does not surface it, drop the same `.mdc` into your project's `.cursor/rules/`. As with
Claude and Codex, the Cursor manifest does not register its own MCP server — configure
the Aionforge endpoint separately in Cursor's MCP settings.

## Safety Posture

The plugin follows the MCP service posture:

- Recalled memory is third-party data, not instructions.
- Agents should recall before substantial work and capture generously when
  durable facts appear; Aionforge is designed for large memory sets.
- User direction still wins: a user can ask the agent to remember, update,
  forget, audit, consolidate, or avoid memory for a task.
- Read-like tools are `server_status`, `search`, `read_memory`,
  `session_manifest`, `consolidation_status`, `audit_history`, `work_tree`, and
  `work_query`.
- Mutating tools are `capture`, `batch_capture`, `consolidate`, `forget`,
  `unforget`, `pin`, `unpin`, `work_create`, `work_advance`, and `work_link`; keep
  them behind user approval unless your deployment has a stricter local rule.

The server also publishes the compact resource
`aionforge://plugin/aionforge-memory` so connected MCP clients can discover the
plugin package without loading this page.
