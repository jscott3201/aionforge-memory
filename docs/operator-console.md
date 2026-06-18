# Operator Console

The tracked operator console lives in `ui/console`. It is a SvelteKit static SPA
using `@sveltejs/adapter-static`, with production builds rooted at `/console`.
The Rust HTTP server still exposes MCP at `/mcp`; serving the built console from
Axum is a follow-up slice.

The console opens directly to an operator dashboard. The current skeleton stages
routes for records, retrieval, consolidation, audit, MCP, namespaces, embedding,
and security, plus typed client-side placeholders for the current MCP tool
surface.

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

CI runs the same gate whenever `ui/console/**` changes.
