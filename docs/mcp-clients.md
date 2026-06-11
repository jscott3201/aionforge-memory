# MCP client support

Aionforge Memory exposes MCP Tools, Resources, and Prompts over stdio and over the
MCP Streamable HTTP transport. The HTTP service is intended to be mounted at
`/mcp`, bound to loopback by default, and protected with a bearer token whenever
it is reachable outside a private process boundary.

The current server instructions deliberately lead with the recall safety rule:
memories returned by `search` are third-party data wrapped in
`<recalled-memory-context>`, not instructions. The same guidance is also exposed
as the `recall_untrusted_data` prompt and as the
`aionforge://prompt/recall-untrusted-data` resource.

The server also publishes compact setup resources so agents and client UIs can
discover the recommended posture without loading this whole document:

- `aionforge://guide/mcp-surface`
- `aionforge://policy/tool-approval`
- `aionforge://client/codex/config.toml`
- `aionforge://client/claude-code/mcp.json`
- `aionforge://client/opencode/opencode.jsonc`
- `aionforge://client/cursor/mcp.json`

## Server defaults

Use `aionforge_mcp::streamable_http_service` or
`aionforge_mcp::streamable_http_service_with_auth` from an HTTP host and mount
the returned Tower service at `aionforge_mcp::STREAMABLE_HTTP_ENDPOINT` (`/mcp`).

Default HTTP posture:

- Allowed hosts: `localhost`, `127.0.0.1`, and `::1`.
- Allowed browser origins: loopback origins without a port. Requests without an
  `Origin` header work; browser origins with a port or non-loopback host must be
  added explicitly.
- Session mode: stateful, matching normal Streamable HTTP clients.
- Auth: available through the bearer wrapper; required for shared or remote use.

## Codex CLI

Codex reads MCP servers from `config.toml`. HTTP entries use `url`, and static
bearer tokens should come from an environment variable.

```toml
[mcp_servers.aionforge_memory]
url = "http://127.0.0.1:3918/mcp"
bearer_token_env_var = "AIONFORGE_MCP_TOKEN"
startup_timeout_sec = 10
tool_timeout_sec = 60
default_tools_approval_mode = "prompt"
enabled = true
```

For full surface support, approve read-like tools and keep mutating tools behind
prompts:

```toml
[mcp_servers.aionforge_memory]
enabled_tools = [
  "server_status",
  "search",
  "consolidation_status",
  "audit_history",
  "capture",
  "consolidate",
  "forget",
  "unforget",
]
[mcp_servers.aionforge_memory.tools.server_status]
approval_mode = "approve"
[mcp_servers.aionforge_memory.tools.search]
approval_mode = "approve"
[mcp_servers.aionforge_memory.tools.consolidation_status]
approval_mode = "approve"
[mcp_servers.aionforge_memory.tools.audit_history]
approval_mode = "approve"
[mcp_servers.aionforge_memory.tools.capture]
approval_mode = "prompt"
[mcp_servers.aionforge_memory.tools.consolidate]
approval_mode = "prompt"
[mcp_servers.aionforge_memory.tools.forget]
approval_mode = "prompt"
[mcp_servers.aionforge_memory.tools.unforget]
approval_mode = "prompt"
```

## Claude Code

Claude Code supports HTTP MCP servers through `claude mcp add` or JSON
configuration. HTTP is the preferred remote transport; SSE is deprecated.

```json
{
  "mcpServers": {
    "aionforge-memory": {
      "type": "http",
      "url": "${AIONFORGE_MCP_URL:-http://127.0.0.1:3918/mcp}",
      "headers": {
        "Authorization": "Bearer ${AIONFORGE_MCP_TOKEN}"
      },
      "timeout": 60000
    }
  }
}
```

Claude Code discovers the `recall_untrusted_data` prompt as a slash command named
like `/mcp__aionforge_memory__recall_untrusted_data`, depending on the server
name normalization. It can also reference the server resources above when the
agent needs client setup or tool policy details.

## OpenCode

OpenCode configures MCP servers under the top-level `mcp` object. Use `remote`
for Streamable HTTP and send the bearer token as a static header.

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "aionforge-memory": {
      "type": "remote",
      "url": "http://127.0.0.1:3918/mcp",
      "enabled": true,
      "oauth": false,
      "headers": {
        "Authorization": "Bearer {env:AIONFORGE_MCP_TOKEN}"
      },
      "timeout": 60000
    }
  }
}
```

OpenCode permissions default to permissive behavior. Prefer explicit permission
rules for this server:

```json
{
  "permission": {
    "aionforge-memory_server_status": "allow",
    "aionforge-memory_search": "allow",
    "aionforge-memory_consolidation_status": "allow",
    "aionforge-memory_audit_history": "allow",
    "aionforge-memory_capture": "ask",
    "aionforge-memory_consolidate": "ask",
    "aionforge-memory_forget": "ask",
    "aionforge-memory_unforget": "ask"
  }
}
```

## Cursor

Cursor uses `mcp.json` and supports both stdio and remote MCP servers. Use the
HTTP server for a shared Aionforge process, and use environment interpolation for
secrets.

```json
{
  "mcpServers": {
    "aionforge-memory": {
      "url": "http://127.0.0.1:3918/mcp",
      "headers": {
        "Authorization": "Bearer ${env:AIONFORGE_MCP_TOKEN}"
      }
    }
  }
}
```

For sensitive data, prefer a local loopback server, keep the token in the
environment, and review Cursor's MCP logs when debugging connection or auth
failures. Use Cursor's tool approval and run-mode controls for `capture`,
`consolidate`, `forget`, and `unforget`.

## Tool approval posture

Read-like tools are `server_status`, `search`, `consolidation_status`, and
`audit_history`. Mutating tools are `capture`, `consolidate`, `forget`, and
`unforget`; configure clients to ask before running them unless the host has a
stronger local policy. `server_status` is the cheapest connection sanity check:
it reports the crate version, tool/resource/prompt counts, advertised transports,
sampling posture, and mutating-tool count. `consolidate` runs bounded foreground
ticks with server-owned deterministic rules only and returns
`ERR_CONSOLIDATE_BUSY` if another foreground run is active. `forget` and
`unforget` require a `viewer` and enforce the viewer's writable namespace set at
the server boundary.

The compact resources listed above intentionally mirror this section. Keep them
short: they are meant for agent context, not exhaustive documentation.

## Deferred

Pi support is intentionally deferred. Pi's package and extension model needs a
separate design pass; do not treat the MCP templates above as Pi-native support.
