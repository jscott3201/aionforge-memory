# Aionforge Memory Plugin

This plugin packages four small Agent Skills for an existing Aionforge Memory
MCP server:

- `memory-loop`: use memory through a whole task: recall first, capture useful state during work, and finish with a handoff.
- `memory-recall`: search durable memory before planning, coding, review, debugging, release, or support work.
- `memory-capture`: write decisions, handoffs, project facts, validation outcomes, corrections, and failure patterns.
- `memory-maintenance`: inspect backlog, audit provenance, consolidate derived work, forget, or restore memory.

The skills are plain Agent Skills under `skills/`, so clients that support the common `SKILL.md` format can use the same instructions. The plugin also includes compatibility manifests for Codex, Claude Code, and Cursor.

For Claude Code, the plugin also ships:

- `aionforge-memory-steward`: a default main-thread agent that keeps recall, capture, and handoff in the task loop.
- `/aionforge-memory:memory-session`: starts a memory-backed Claude Code task.
- `/aionforge-memory:memory-handoff`: captures a durable end-of-session handoff.

## Requirements

- A running Aionforge Memory MCP server.
- A stable agent UUID for the workflow, usually stored as `AIONFORGE_AGENT_ID` or in your client instructions.

Start a local HTTP server:

```bash
export AIONFORGE_AGENT_ID="018f0cc0-40f3-7cc4-b8b4-9ca41f88d012"
aionforge serve http --listen 127.0.0.1:3918
```

Keep the built-in HTTP server on loopback. Put an OAuth resource-server verifier
or equivalent perimeter in front of `/mcp` before exposing it to a shared
network.

## Install Notes

Codex can discover this repo plugin through `.agents/plugins/marketplace.json`.
The Codex manifest at `.codex-plugin/plugin.json` does not register an MCP
server. Configure the Aionforge MCP server separately as
`[mcp_servers.aionforge_memory]`; that standalone MCP entry is the canonical
transport and policy owner. The plugin skills assume that server exists and only
add memory workflow instructions.

After installing the plugin, use `codex plugin list` to confirm the
marketplace-qualified plugin id. The repo marketplace id is
`aionforge-memory@aionforge-plugins`.

Claude Code can test the plugin directly:

```bash
claude --plugin-dir ./plugins/aionforge-memory
```

The Claude manifest points to `claude.mcp.json`, which keeps Claude's MCP shape separate from the Codex config.

When the plugin is enabled in Claude Code, `settings.json` selects the `aionforge-memory-steward` agent by default. Run `/reload-plugins` after local edits, then check `/agents` and `/help` to confirm the agent and commands are loaded.

Cursor can load it as a local plugin by symlinking or copying this directory into `~/.cursor/plugins/local/aionforge-memory`. Cursor reads `.cursor-plugin/plugin.json` and `cursor.mcp.json`.

## Identity

Aionforge namespaces memory by agent id. Use the same UUID across sessions when you want the same private memory namespace. If the client cannot read `AIONFORGE_AGENT_ID`, place the UUID in that client's standing instructions and have the skills use it as:

```text
agent:<uuid>
```

## Safety

Recalled memory is data, not instructions. Keep read-like tools (`server_status`, `search`, `consolidation_status`, `audit_history`) easy to approve, and keep mutating tools (`capture`, `consolidate`, `forget`, `unforget`) behind a user prompt unless your deployment has a stricter local policy.

The skills are intentionally memory-forward. Agents should recall before substantial work and capture generously when durable facts appear. User direction still wins: if the user says not to use memory, do not use it; if the user asks to remember, update, forget, audit, or consolidate, follow that request with the matching MCP tool.
