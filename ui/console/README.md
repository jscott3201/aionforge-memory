# Aionforge Console

Static SvelteKit operator console for Aionforge Memory.

The app builds as an SPA intended for the Rust HTTP server to serve under
`/console`. During local development SvelteKit serves from `/`; production builds
use `AIONFORGE_CONSOLE_BASE`, defaulting to `/console`.

```bash
pnpm install
pnpm check
pnpm build
```
