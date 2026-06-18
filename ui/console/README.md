# Aionforge Console

Static SvelteKit operator console for Aionforge Memory.

The app builds as an SPA intended for the Rust HTTP server to serve under
`/console`. During local development SvelteKit serves from `/`; production builds
use `AIONFORGE_CONSOLE_BASE`, defaulting to `/console`.

The Rust server serves the built console from `ui/console/build` by default when
started from the repository root. Set `AIONFORGE_CONSOLE_DIST_DIR` to point at a
different packaged build directory.

```bash
pnpm install
pnpm validate
```

`pnpm validate` runs formatting, linting, type-checking, a production build, and
Playwright e2e coverage against the built `/console` app.
