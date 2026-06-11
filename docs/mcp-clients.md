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

- `aionforge://manifest/tools.json`
- `aionforge://guide/mcp-surface`
- `aionforge://policy/tool-approval`
- `aionforge://client/oauth-guide`
- `aionforge://plugin/aionforge-memory`
- `aionforge://client/codex/config.toml`
- `aionforge://client/claude-code/mcp.json`
- `aionforge://client/opencode/opencode.jsonc`
- `aionforge://client/cursor/mcp.json`

## Server defaults

Use `aionforge_mcp::streamable_http_service` or
`aionforge_mcp::streamable_http_service_with_auth` from an HTTP host and mount
the returned Tower service at `aionforge_mcp::STREAMABLE_HTTP_ENDPOINT` (`/mcp`).
The single `aionforge` binary can serve the same surface directly:

```bash
aionforge serve stdio
AIONFORGE_AGENT_ID=018f0cc0-40f3-7cc4-b8b4-9ca41f88d012 \
AIONFORGE_MCP_TOKEN=change-me \
  aionforge serve http --listen 127.0.0.1:3918 \
  --bearer-token-agent-env AIONFORGE_AGENT_ID=AIONFORGE_MCP_TOKEN
```

The Docker image defaults to the authenticated HTTP server on port `3918` and
stores state under `/data`:

```bash
docker run --rm \
  -e AIONFORGE_AGENT_ID=018f0cc0-40f3-7cc4-b8b4-9ca41f88d012 \
  -e AIONFORGE_MCP_TOKEN=change-me \
  -p 127.0.0.1:3918:3918 \
  -v aionforge-data:/data \
  aionforge-memory:dev
```

On Apple silicon Macs running macOS 26, use the same HTTP endpoint shape with
Apple's `container` runtime. The commands and local lifecycle helper live in
[Apple container](apple-container.md).

Default HTTP posture:

- Allowed hosts: `localhost`, `127.0.0.1`, and `::1`.
- Allowed browser origins: loopback origins without a port. Requests without an
  `Origin` header work; browser origins with a port or non-loopback host must be
  added explicitly.
- Request body limit: 1 MiB by default through
  `StreamableHttpOptions::max_request_body_bytes`; oversized requests return
  `413 Payload Too Large`.
- Session mode: stateful, matching normal Streamable HTTP clients.
- Auth: the CLI requires principal-bound bearer tokens for HTTP. Each token
  authenticates one agent id, and identity-bearing tools reject mismatched
  `agent_id` or `viewer` values.

## OAuth Readiness

The built-in bearer wrapper is a local/private deployment guard, not a complete
OAuth resource server. For remote multi-user deployments, mount an OAuth verifier
at the HTTP boundary that validates issuer, expiry, scope, and audience/resource
binding before the MCP service sees the request. If the verifier forwards to the
`aionforge` CLI server, it must replace the inbound `Authorization` header with a
configured internal principal-bound bearer token. Custom hosts can instead mount
the library service behind the verifier. Do not pass inbound MCP access tokens
through to downstream services.

The crate exposes two small helpers for MCP OAuth 2.1 integration:

- `OAuthProtectedResourceMetadata` renders RFC 9728 metadata for the MCP endpoint.
  For the default `/mcp` path, serve it at
  `/.well-known/oauth-protected-resource/mcp` or expose the same URL through the
  `WWW-Authenticate` challenge.
- `BearerAuthChallenge` can advertise `resource_metadata` and `scope` parameters
  on 401 responses so SDK clients can discover the authorization server and
  request the right scopes.

Use the MCP endpoint URL as the OAuth `resource` value, for example
`https://memory.example.com/mcp`. Authorization and token requests should include
that resource value, and the verifier should reject tokens that are not audience
bound to it.

When running the built-in HTTP server behind an OAuth verifier, pass the public
endpoint URL and issuer metadata so MCP clients can discover the protected
resource:

```bash
AIONFORGE_AGENT_ID=018f0cc0-40f3-7cc4-b8b4-9ca41f88d012 \
AIONFORGE_MCP_TOKEN=change-me \
aionforge serve http --listen 127.0.0.1:3918 \
  --bearer-token-agent-env AIONFORGE_AGENT_ID=AIONFORGE_MCP_TOKEN \
  --public-url https://memory.example.com/mcp \
  --oauth-issuer https://auth.example.com \
  --oauth-scope memory.read --oauth-scope memory.write
```

That serves RFC 9728 protected-resource metadata at
`/.well-known/oauth-protected-resource/mcp` and advertises that metadata URL in
the 401 `WWW-Authenticate` challenge. Token validation is still the job of the
upstream OAuth verifier; the built-in bearer wrapper checks the bearer value that
reaches the MCP service and binds it to the configured agent id.

Static bearer and OAuth modes should not be mixed in client config. A static
`Authorization` header is appropriate for loopback/private deployments. For
OAuth deployments, omit static authorization headers so the client can discover
the protected-resource metadata, run its authorization flow, and request tokens
for the MCP endpoint resource.

This guidance tracks the public MCP authorization spec and the current client
docs for Codex, Claude Code, OpenCode, and Cursor:

- https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization
- https://developers.openai.com/codex/mcp
- https://code.claude.com/docs/en/mcp
- https://opencode.ai/docs/mcp-servers/
- https://cursor.com/docs/mcp.md

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

For OAuth deployments, remove `bearer_token_env_var` and run:

```bash
codex mcp login aionforge_memory
```

If the authorization server requires a fixed redirect URI, set Codex's top-level
`mcp_oauth_callback_port` and, when needed, `mcp_oauth_callback_url`. Codex uses
server-advertised `scopes_supported` when available, so keep
`--oauth-scope` values tight and stable.

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

When the MCP server comes from the installed Codex plugin rather than a
standalone `[mcp_servers]` entry, keep policy under the plugin-scoped table:

```toml
[plugins."aionforge-memory@aionforge-plugins".mcp_servers.aionforge_memory]
enabled = true
default_tools_approval_mode = "prompt"
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
[plugins."aionforge-memory@aionforge-plugins".mcp_servers.aionforge_memory.tools.server_status]
approval_mode = "approve"
[plugins."aionforge-memory@aionforge-plugins".mcp_servers.aionforge_memory.tools.search]
approval_mode = "approve"
[plugins."aionforge-memory@aionforge-plugins".mcp_servers.aionforge_memory.tools.consolidation_status]
approval_mode = "approve"
[plugins."aionforge-memory@aionforge-plugins".mcp_servers.aionforge_memory.tools.audit_history]
approval_mode = "approve"
[plugins."aionforge-memory@aionforge-plugins".mcp_servers.aionforge_memory.tools.capture]
approval_mode = "prompt"
[plugins."aionforge-memory@aionforge-plugins".mcp_servers.aionforge_memory.tools.consolidate]
approval_mode = "prompt"
[plugins."aionforge-memory@aionforge-plugins".mcp_servers.aionforge_memory.tools.forget]
approval_mode = "prompt"
[plugins."aionforge-memory@aionforge-plugins".mcp_servers.aionforge_memory.tools.unforget]
approval_mode = "prompt"
```

If the marketplace name differs, replace `aionforge-plugins` with the name shown
by `codex plugin list`.

## Claude Code

Claude Code supports HTTP MCP servers through `claude mcp add` or JSON
configuration. HTTP is the preferred remote transport; SSE is deprecated. The
JSON `type` may be `http` or the MCP transport name `streamable-http`.

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

Claude Code's tool search keeps MCP tool schemas out of the initial context by
default and uses the server `instructions` field to decide when to discover
tools, so Aionforge keeps instructions compact and front-loads the recall safety
rule. For OAuth deployments, configure the HTTP URL without the `Authorization`
header and authenticate from `/mcp`; if a static header is present and rejected,
Claude Code treats the connection as failed instead of falling back to OAuth.

## OpenCode

OpenCode configures MCP servers under the top-level `mcp` object. Use `remote`
for Streamable HTTP and send the bearer token as a static header. Set
`oauth: false` for static bearer mode so OpenCode does not try OAuth discovery
for a token-protected local endpoint.

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

For OAuth deployments, omit `headers`; OpenCode will detect the 401 challenge
and can use dynamic client registration. If the provider requires a
pre-registered client, set `oauth.clientId`, `oauth.clientSecret`, and `oauth.scope`.

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

Cursor uses the `mcp.json` shape and supports both stdio and remote MCP
servers. Use the HTTP server for a shared Aionforge process, and use
environment interpolation for secrets. The repository plugin stores this Cursor
template as `cursor.mcp.json` so Codex does not ingest it as a second MCP
server.

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

For pre-registered OAuth clients, Cursor uses an `auth` object on remote URL
entries with `CLIENT_ID`, optional `CLIENT_SECRET`, and optional `scopes`.
Cursor's fixed MCP OAuth redirect URL is
`cursor://anysphere.cursor-mcp/oauth/callback`; register it with providers that
require redirect allow-listing. For dynamic OAuth discovery, omit the static
bearer header.

## Tool approval posture

Read-like tools are `server_status`, `search`, `consolidation_status`, and
`audit_history`. `audit_history` reads the principal-scoped audit subgraph by
subject, by `kind`, or by subject+kind; when `subject_id` is omitted, `kind` is
required and the compact output uses `subject=*` while listing each row's
subject. Mutating tools are `capture`, `consolidate`, `forget`, and
`unforget`; configure clients to ask before running them unless the host has a
stronger local policy. `server_status` is the cheapest connection sanity check:
it reports the crate version, tool/resource/prompt counts, advertised transports,
sampling posture, and mutating-tool count. `consolidate` runs bounded foreground
ticks with server-owned deterministic rules only and returns
`ERR_CONSOLIDATE_BUSY` if another foreground run is active. `forget` and
`unforget` require a `viewer` and enforce the viewer's writable namespace set at
the server boundary.

`aionforge://manifest/tools.json` is the lowest-token machine-readable contract
for agents. It lists the server version, tool classes, recommended approval
posture, MCP tool annotation hints, compact output shape, and stable `ERR_*`
markers. The server sets `readOnlyHint=true` for read-like tools,
`openWorldHint=false` for the local memory surface, and `destructiveHint=true`
for `forget`; treat those as client-routing hints and keep the approval policy as
the enforcement rule. The other compact resources intentionally mirror this
section; keep them short because they are meant for agent context, not exhaustive
documentation.

The repo also ships an installable plugin package at
[`plugins/aionforge-memory`](../plugins/aionforge-memory). The MCP resource
`aionforge://plugin/aionforge-memory` gives connected clients the compact
version of that setup path.

## Deferred

Pi support is intentionally deferred. Pi's package and extension model needs a
separate design pass; do not treat the MCP templates above as Pi-native support.
