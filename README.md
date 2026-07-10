<h1 align="center">
  <img src=".github/assets/logo.svg" alt="Aionforge Memory" width="640">
</h1>

<p align="center">
  Long-term memory for AI agents, built on selene-db.
</p>

> **Status: 0.3.0 public release.** Aionforge Memory is public and usable,
> but still pre-1.0. Expect schema and API changes before 1.0. The 0.3.0
> release is a fresh-store release from 0.2.x because the selene-db 1.2 to 1.3
> upgrade changes the WAL/schema format.

Aionforge Memory gives agents a durable memory store they can recall across
sessions. It stores captured episodes, derived facts and notes, procedural
memory, work items, provenance, and audit events in
[`selene-db`](https://github.com/jscott3201/selene-db), then recalls relevant
context with lexical search, vector search, graph signals, recency, importance,
and trust-aware ranking.

Use it when you want an agent or a team of agents to remember decisions,
handoffs, failures, procedures, project facts, and open work without treating
recalled text as new instructions.

## Quick Start

These commands build the local binary and start an MCP server on loopback.
Embedding is disabled in this first config so you can verify the server without
running an OpenAI-compatible embedding provider.

```bash
cargo build --locked --release -p aionforge-cli

mkdir -p .aionforge
cat > .aionforge/config.toml <<'TOML'
[persistence]
data_dir = ".aionforge/data"

[embedder]
enabled = false
TOML

./target/release/aionforge --config .aionforge/config.toml doctor
./target/release/aionforge --config .aionforge/config.toml \
  serve http --listen 127.0.0.1:3918
```

Then point your MCP client at:

```text
http://127.0.0.1:3918/mcp
```

For production-quality semantic recall, configure embeddings instead of leaving
them disabled. Start with the [embedding guide](docs/embedding-guide.md).

## What You Get

- Durable capture of agent observations, decisions, handoffs, and failures.
- Hybrid recall across lexical matches, vectors, graph expansion, recency,
  importance, and trust signals.
- Explicit agent-private, team, global, and system namespaces.
- Provenance and audit records for writes.
- A single `aionforge` binary with `doctor`, `recover`, and `serve`.
- MCP over stdio or Streamable HTTP.
- A repo-shipped agent plugin with memory workflow skills for Codex, Claude
  Code, Cursor, and compatible clients.

Aionforge Memory is retrieval memory, not model training. It does not fine-tune
models or execute recalled content as instructions. See
[honest scope](docs/honest-scope.md) for the current boundaries and deferred
work.

## Memory Model

A capture becomes one immutable episode. Consolidation adds derived facts,
entities, and notes beside that episode instead of rewriting it. Recall returns
a bounded, explicitly untrusted context bundle; lifecycle operations such as
forgetting, erasure, promotion, and demotion are explicit controls.

For the full model, see [Data model and mental model](docs/data-model.md).

## Configure A Client

The server publishes MCP tools, resources, and prompts. For a local HTTP server,
most clients only need the endpoint URL above plus a stable agent UUID.

Client-specific setup lives in [MCP client support](docs/mcp-clients.md):

- Codex CLI
- Claude Code
- OpenCode
- Cursor

The important safety rule is simple: recalled memory is wrapped as
third-party data and should be treated as context, not instruction text.

## Use The Agent Plugin

The plugin at [plugins/aionforge-memory](plugins/aionforge-memory/README.md)
adds reusable Agent Skills for the memory workflow:

- recall before substantial work
- capture durable facts as they happen
- track tasks and blockers as durable work items
- finish sessions with a handoff

The plugin does not start or register an MCP server by itself. Run the
`aionforge` MCP server separately, then configure the plugin-enabled client to
use that server.

See [Agent plugin](docs/plugins.md) for install and identity setup.

## Run With Docker

Published images are available for `linux/amd64` and `linux/arm64`:

```bash
docker pull ghcr.io/jscott3201/aionforge-memory:0.3.0
```

Run a local smoke-test server with embeddings disabled:

```bash
docker run --rm \
  -p 127.0.0.1:3918:3918 \
  -v aionforge-data:/data \
  -e AIONFORGE_EMBEDDER__ENABLED=false \
  ghcr.io/jscott3201/aionforge-memory:0.3.0
```

For bind mounts, use an owner-only data directory. The container runs as
UID/GID `10001:10001`, and the store refuses unsafe data directory permissions.
Operations details are in [Operations and recovery](docs/operations-recovery.md).

## Use The Rust Library

Rust hosts can link the `aionforge` crate directly and provide an `Embedder`
implementation:

```rust
use aionforge::{CaptureRequest, Embedder, Memory, MemoryConfig, Principal, RecallQuery};

# async fn run<E: Embedder>(embedder: E) -> Result<(), Box<dyn std::error::Error>> {
let now = "2026-06-06T09:30:00-05:00[America/Chicago]".parse()?;
let memory = Memory::open_in_memory(embedder, &now, MemoryConfig::default())?;

let viewer = Principal::agent("0197b0aa-3c5e-8000-8000-000000000001".parse()?);
let bundle = memory.search(RecallQuery::new("graph databases", viewer, 5)).await?;
println!("{}", bundle.rendered);
# Ok(())
# }
```

For complete call shapes, see [crates/aionforge/src/lib.rs](crates/aionforge/src/lib.rs)
and the integration tests under [crates/aionforge/tests](crates/aionforge/tests).

## Documentation

Start here:

- [Getting started](docs/getting-started.md) - build, configure, validate, and run.
- [Data model and mental model](docs/data-model.md) - what gets stored and recalled.
- [Embedding guide](docs/embedding-guide.md) - providers, dimensions, and secrets.
- [MCP client support](docs/mcp-clients.md) - Codex, Claude Code, OpenCode, Cursor.
- [Agent plugin](docs/plugins.md) - skills, identity, and client notes.
- [Security model](docs/security-model.md) - namespaces, untrusted recall, signing.
- [Operations and recovery](docs/operations-recovery.md) - production setup and WAL recovery.
- [Honest scope](docs/honest-scope.md) - what is shipped, experimental, or deferred.

The full subsystem map is in [docs/README.md](docs/README.md).

## Contributing

This project is public and pre-1.0. Issues and pull requests are welcome.
Open an issue before large design changes.

- [CONTRIBUTING.md](CONTRIBUTING.md) covers setup, branch flow, commit style, and local gates.
- [AGENTS.md](AGENTS.md) covers crate layering, invariants, and agent-facing validation.
- Use the [issue chooser](../../issues/new/choose) for bugs, features, and design proposals.

Do not include private planning notes, secrets, internal handoff text, or agent
transcripts in public issues or PRs.

## License

Dual-licensed under either [Apache 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT),
at your option. Contributions are accepted under the same dual license unless
stated otherwise.
