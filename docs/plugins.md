# Agent Plugin

Aionforge Memory ships a plugin package at
[`plugins/aionforge-memory`](../plugins/aionforge-memory). It bundles four Agent
Skills with MCP configuration for Codex, Claude Code, and Cursor.

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
- Claude Code agent `aionforge-memory-steward`: keeps recall, capture, and
  handoff in the main task loop when the plugin is enabled.
- Claude Code commands `/aionforge-memory:memory-session` and
  `/aionforge-memory:memory-handoff`: explicit workflows for starting a
  memory-backed task and ending with a durable handoff.
- Client manifests for Codex, Claude Code, and Cursor.
- MCP config files for the local HTTP endpoint at `http://127.0.0.1:3918/mcp`.

## Identity Setup

The MCP tools require an explicit agent identity. Use one stable UUID across
sessions when you want the same private memory namespace.

Recommended local setup:

```bash
export AIONFORGE_AGENT_ID="<uuid>"
export AIONFORGE_MCP_TOKEN="$(openssl rand -hex 32)"
aionforge serve http --listen 127.0.0.1:3918 \
  --bearer-token-agent-env AIONFORGE_AGENT_ID=AIONFORGE_MCP_TOKEN
```

If a client cannot read `AIONFORGE_AGENT_ID`, put the UUID in that client's
standing instructions and have the skills use `agent:<uuid>` for recall and the
raw UUID for capture.

## Client Notes

Codex can discover the plugin from the repo-scoped
`.agents/plugins/marketplace.json`. The Codex plugin manifest points to
`.mcp.json`. After installation, `codex plugin list` shows the
marketplace-qualified plugin id. For the repo marketplace, the id is
`aionforge-memory@aionforge-plugins`.

The package root `plugin.json` also points at `.mcp.json`. That keeps Codex on
one server id, `aionforge_memory`, with `bearer_token_env_var` auth instead of
also loading a second unauthenticated or header-style entry from a generic MCP
manifest.

Use `plugins/aionforge-memory/codex.plugin-policy.example.toml` as the Codex
config shape when you want plugin-scoped MCP policy. It keeps read-like tools
approved and mutating tools prompted under
`plugins."aionforge-memory@aionforge-plugins".mcp_servers.aionforge_memory`.

Claude Code can test the package directly:

```bash
claude --plugin-dir ./plugins/aionforge-memory
```

The Claude manifest loads `skills/`, `commands/`, `agents/`, and
`claude.mcp.json`. The plugin root `settings.json` selects
`aionforge-memory-steward` as the default main-thread agent so ordinary Claude
Code work starts with the memory loop available.

Cursor can load the package as a local plugin from
`~/.cursor/plugins/local/aionforge-memory`; it reads `.cursor-plugin/plugin.json`
and `cursor.mcp.json`.

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
