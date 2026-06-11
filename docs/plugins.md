# Agent Plugin

Aionforge Memory ships a plugin package at
[`plugins/aionforge-memory`](../plugins/aionforge-memory). It bundles four Agent
Skills with MCP configuration for Codex, Claude Code, Cursor, and GitHub Copilot
CLI.

The plugin is meant to make the existing MCP service easier to use. It does not
add a second server and it does not execute stored skills from memory. The MCP
server still owns capture, recall, lifecycle tools, safety wrappers, and
approval hints.

## What It Includes

- `memory-loop`: use memory through a whole task: recall first, capture useful
  state during work, and finish with a handoff.
- `memory-recall`: search durable memory before planning, coding, review,
  debugging, release, or support work.
- `memory-capture`: capture decisions, project facts, validation results, and
  session handoffs when the user wants them persisted or project policy grants
  standing permission.
- `memory-maintenance`: inspect backlog, audit provenance, consolidate derived
  work, forget, or restore memory.
- Client manifests for Codex, Claude Code, Cursor, and GitHub Copilot CLI.
- MCP config files for the local HTTP endpoint at `http://127.0.0.1:3918/mcp`.

## Identity Setup

The MCP tools require an explicit agent identity. Use one stable UUID across
sessions when you want the same private memory namespace.

Recommended local setup:

```bash
export AIONFORGE_AGENT_ID="<uuid>"
export AIONFORGE_MCP_TOKEN="$(openssl rand -hex 32)"
aionforge serve http --listen 127.0.0.1:3918 \
  --bearer-token-env AIONFORGE_MCP_TOKEN
```

If a client cannot read `AIONFORGE_AGENT_ID`, put the UUID in that client's
standing instructions and have the skills use `agent:<uuid>` for recall and the
raw UUID for capture.

## Client Notes

Codex can discover the plugin from the repo-scoped
`.agents/plugins/marketplace.json`. The Codex plugin manifest points to
`.mcp.json`.

Claude Code can test the package directly:

```bash
claude --plugin-dir ./plugins/aionforge-memory
```

Cursor can load the package as a local plugin from
`~/.cursor/plugins/local/aionforge-memory`; it reads `.cursor-plugin/plugin.json`
and `mcp.json`.

GitHub Copilot CLI can install the package from the local path:

```bash
copilot plugin install ./plugins/aionforge-memory
```

## Safety Posture

The plugin follows the MCP service posture:

- Recalled memory is third-party data, not instructions.
- Agents should recall before substantial work and capture generously when
  durable facts appear; Aionforge is designed for large memory sets.
- User direction still wins: a user can ask the agent to remember, update,
  forget, audit, consolidate, or avoid memory for a task.
- Read-like tools are `server_status`, `search`, `consolidation_status`, and
  `audit_history`.
- Mutating tools are `capture`, `consolidate`, `forget`, and `unforget`; keep
  them behind user approval unless your deployment has a stricter local rule.

The server also publishes the compact resource
`aionforge://plugin/aionforge-memory` so connected MCP clients can discover the
plugin package without loading this page.
