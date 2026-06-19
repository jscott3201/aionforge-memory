# Operator Console

The tracked operator console lives in `ui/console`. It is a SvelteKit static SPA
using `@sveltejs/adapter-static`, with production builds rooted at `/console`.
The Rust HTTP server exposes MCP at `/mcp` and serves the built console under
`/console` when a console build directory is available.

The console opens directly to an operator dashboard. The current skeleton stages
routes for records, retrieval, consolidation, audit, MCP, namespaces, embedding,
and security, plus typed client-side placeholders for the current MCP tool
surface.

The MCP server is still the console API. Read-like MCP tools keep their compact
text output for agent clients and attach `structuredContent` DTOs for the console:
`server_status`, `consolidation_status`, `search`, `read_memory`,
`session_manifest`, `audit_history`, `work_query`, and `work_tree` all have typed
schemas mirrored in `ui/console/src/lib/api/contracts.ts`.

## Validation

Console changes are held to the same repository hygiene as Rust code: formatted
source, linting, type-checking, production build, and the repository 700-line
source-file cap.

```bash
cd ui/console
pnpm install --frozen-lockfile
pnpm validate
```

`pnpm validate` runs:

- `pnpm format:check`
- `pnpm lint`
- `pnpm check`
- `pnpm build`
- `pnpm e2e`

CI runs the same gate whenever `ui/console/**` changes, including Chromium
Playwright coverage for `/console` and a deep-linked console route.

As API-backed views land, add Playwright coverage against a live `aionforge`
server with seeded memory state where practical. Mock-only UI tests are not
enough for dashboard, records, retrieval, consolidation, or audit flows whose
value depends on real MCP data.

## Serving

Build the console before starting the Rust HTTP server from the repository root:

```bash
cd ui/console
pnpm build
cd ../..
aionforge serve http --listen 127.0.0.1:3918
```

By default the server checks these filesystem layouts, in order:

- `ui/console/build` for local source checkouts
- `console` next to the running `aionforge` executable for release archives
- `./console` for manually staged assets
- `/usr/local/share/aionforge/console` for container images

Set `AIONFORGE_CONSOLE_DIST_DIR` to serve a packaged build directory from
another location. If no shell is present, `/console` is not mounted and the
server keeps returning the normal plain `404` for non-MCP paths.

Release tarballs include both the `aionforge` binary and a sibling `console/`
directory. Docker and GHCR runtime images copy the console to
`/usr/local/share/aionforge/console` and set `AIONFORGE_CONSOLE_DIST_DIR`
inside the image.
