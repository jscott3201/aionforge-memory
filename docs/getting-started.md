# Getting started

This guide is the shortest path from a fresh checkout to a local Aionforge
Memory process that a Rust host or MCP client can use. For subsystem details,
follow the links in [the docs index](README.md).

## Build from source

Aionforge Memory is a Rust workspace pinned to the toolchain in
`rust-toolchain.toml`. It depends on the private `selene-db` repository through
SSH, so Cargo must be able to fetch from GitHub with a key that has access.

```bash
cargo build --workspace --locked
cargo nextest run --workspace --locked --all-features
```

The main operator binary is `aionforge` from the `aionforge-cli` crate. It can
run health checks, validate recovery, and serve the MCP surface over stdio or
Streamable HTTP.

## Configure storage and providers

Configuration is layered in this order: compiled defaults, TOML file,
`AIONFORGE_` environment variables, then command-line flags. A minimal local
file can be as small as:

```toml
[persistence]
data_dir = "/tmp/aionforge-memory"

[embedder]
enabled = true
endpoint = "http://127.0.0.1:1234/v1"
model = "codestral-embed-2505"
dimension = 1536
```

For production, start from [the production example](../examples/production.toml)
and replace the paths, model ids, and secret-manager environment names. Secrets
never belong in TOML; config stores only environment variable names such as
`api_key_env`.

The data directory is security-sensitive. On Unix, Aionforge creates a fresh
directory as `0700` and refuses symlink final paths or existing directories that
are readable by group or other users.

## Validate the store

Run `doctor` before exposing the MCP server:

```bash
aionforge --config /path/to/config.toml doctor
aionforge --config /path/to/config.toml doctor --json
```

`doctor` opens the configured store, creating a fresh migrated store if no WAL
exists yet. It reports schema, native index/provider inventory, embedder
dimension binding, consolidation lag, and graph capacity.

Use `recover` only when validating an existing persisted store:

```bash
aionforge --config /path/to/config.toml recover --json
```

`recover` refuses a missing WAL instead of creating a new store, replays the
WAL, then emits the same typed doctor snapshot.

## Serve MCP

For a local agent process, stdio is the smallest surface:

```bash
aionforge --config /path/to/config.toml serve stdio
```

For a shared local service, use Streamable HTTP with a bearer token from the
environment:

```bash
export AIONFORGE_MCP_TOKEN="$(openssl rand -hex 32)"
aionforge --config /path/to/config.toml \
  serve http --listen 127.0.0.1:3918 \
  --bearer-token-env AIONFORGE_MCP_TOKEN
```

Then configure your client with the MCP endpoint
`http://127.0.0.1:3918/mcp`. The setup snippets for Codex CLI, Claude Code,
OpenCode, and Cursor are in [MCP client support](mcp-clients.md) and are also
published by the server as compact `aionforge://client/...` resources.

## Run in Docker

The repository Dockerfile builds the binary with an Alpine builder and runs it
as UID/GID `10001` in an Alpine runtime image:

```bash
DOCKER_BUILDKIT=1 docker build --ssh default -t aionforge-memory:dev .
docker run --rm \
  -e AIONFORGE_MCP_TOKEN=change-me \
  -p 127.0.0.1:3918:3918 \
  -v aionforge-data:/data \
  aionforge-memory:dev
```

For bind mounts, create the host directory as UID/GID `10001:10001` and mode
`0700` before starting the container.

## Use the Rust library

Rust hosts can link the `aionforge` crate directly and provide an implementation
of the `Embedder` trait. The public API re-exports the `Memory` facade and the
domain types used in its signatures:

```rust
use aionforge::{Memory, MemoryConfig, RecallQuery};
use aionforge::{CaptureRequest, Embedder, Principal};

# async fn run<E: Embedder>(embedder: E) -> Result<(), Box<dyn std::error::Error>> {
let now = "2026-06-06T09:30:00-05:00[America/Chicago]".parse()?;
let memory = Memory::open_in_memory(embedder, &now, MemoryConfig::default())?;

// Fill CaptureRequest with the writer, namespace, role, and captured_at data
// your host already knows, then call memory.capture(request).await.
let viewer = Principal::agent("0197b0aa-3c5e-8000-8000-000000000001".parse()?);
let bundle = memory.search(RecallQuery::new("graph databases", viewer, 5)).await?;
println!("{}", bundle.rendered);
# Ok(())
# }
```

See the crate-level docs in `crates/aionforge/src/lib.rs` and the integration
tests under `crates/aionforge/tests/` for complete call shapes.
