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

For tight tool policy, start with only the read path approved:

```toml
enabled_tools = ["search"]
[mcp_servers.aionforge_memory.tools.search]
approval_mode = "approve"
[mcp_servers.aionforge_memory.tools.capture]
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
name normalization.

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

OpenCode can enable MCP tools globally or per agent. Keep mutating tools such as
`capture` behind approval until the host's policy is settled.

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
failures.

## Deferred

Pi support is intentionally deferred. Pi's package and extension model needs a
separate design pass; do not treat the MCP templates above as Pi-native support.
