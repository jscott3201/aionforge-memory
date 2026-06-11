<h1 align="center">
  <img src=".github/assets/logo.svg" alt="Aionforge Memory" width="640">
</h1>

<p align="center">
  Long-term memory for AI agents, built on selene-db.
</p>

> **Status: pre-alpha.** The design is settled and the build is underway, milestone
> by milestone. Schema, APIs, and surfaces will still move around. Nothing here is
> released yet.

Aionforge Memory gives AI agents a real long-term memory: one that remembers across
sessions, keeps its facts straight over time, and can be shared safely between
agents. It's a Rust library built on [`selene-db`](https://github.com/Aionforge-Labs/selene-db),
an embeddable graph engine, and it uses that engine's own storage, search, and graph
machinery instead of bolting on a separate database or vector index.

Three things it's good at:

- A single agent stops forgetting what happened last session.
- A team of agents shares one memory plane, and every write carries provenance, so
  you always know who said what.
- Agents keep a library of skills (procedures that worked) and bad patterns (ones
  that didn't).

You can use it two ways. Link the Rust library straight into your host for the
lowest latency, or run it as an [MCP](https://modelcontextprotocol.io) server over
stdio or HTTP and point a harness at it (Claude Code, Codex, Copilot, Cursor,
OpenCode). There's also a read-only [ratatui](https://ratatui.rs) terminal UI for
watching what the memory is doing, plus a single CLI binary. It runs on macOS and
Linux, natively or in Docker.

## What it is, and what it isn't

This is retrieval memory, not learning. It makes an agent recall better, stay
temporally accurate, follow multi-hop connections, hold together across sessions,
and waste fewer tokens doing it. It does not make the underlying model smarter, and
it does not train or fine-tune anything. It runs no inference of its own either;
embeddings (and optional extraction or reranking) come from an OpenAI-compatible
endpoint you point it at.

We'd rather say that plainly up front than oversell it.

## How it's built

- **Everything goes through the engine.** Storage, BM25 text search, dense vectors,
  and graph algorithms are all selene-db. No second search engine, no separate vector
  store.
- **Bring your own model, one at a time.** Embeddings and the optional chat model go
  through OpenAI-compatible / Anthropic clients (local or hosted); a deployment declares a
  single provider and model, with no cost-first auto-routing, so the responding model stays
  verifiable. The substrate runs no inference itself.
- **Time is first-class, and nothing gets thrown away.** Facts record when they were
  true and when we learned them. A correction supersedes the old fact instead of
  overwriting it. Hard deletion is its own deliberate, audited path.
- **Writes split into two lanes.** Capture is fast, on the order of milliseconds, so
  it never blocks the agent. The slower work (pulling out facts, resolving entities,
  summarizing, and — only when turned on — inducing a reusable skill from a procedure an
  agent keeps repeating) happens in the background.
- **Retrieval picks its strategy per query.** Lexical, dense, graph, recency, and
  trust signals get rank-fused, and graph expansion only kicks in for the queries it
  actually helps.
- **Security isn't a later milestone.** Provenance, optional Ed25519 signed writes,
  per-writer trust folded from an audit log of how each agent's facts held up — which
  re-ranks recall and can un-promote a fact once its attesters decay — namespace
  boundaries, quorum-gated promotion of a team fact to global behind signed attestations
  and a sybil-bounded posterior, quarantine when a new fact contradicts a trusted one,
  tagging recalled text as untrusted data, keeping system-role content out of recall, and
  a CI-gated red-team suite with structured reports are all in scope for v1.
- **Same input, same output.** Given the same graph state, retrieval returns the same
  ordering every time, and derived state can always be rebuilt from the primary graph.
  The optional LLM layers — the distiller that condenses facts into notes, and the link evolver
  that relates notes to each other — are the only places a generative model touches stored content.
  Both run off the consolidation cursor and write only non-canonical state, so turning them on can't
  perturb that byte-for-byte path. They're off by default and degrade to the rule tier.

## Building

You'll need the toolchain pinned in `rust-toolchain.toml` (Rust 1.95.0, edition 2024).

Aionforge builds on `selene-db`, which is a private repo pinned as a git dependency on
its `development` branch (see the root `Cargo.toml`). Cargo fetches it over SSH using
your own key — `.cargo/config.toml` sets `git-fetch-with-cli = true`, so the fetch goes
through your system `git` and SSH agent. You need read access to the `selene-db` repo and
an SSH key GitHub recognizes; nothing extra to clone.

```bash
cargo build --workspace --locked
cargo nextest run --workspace --locked --all-features
```

Run the MCP server from the single binary:

```bash
aionforge serve stdio
AIONFORGE_MCP_TOKEN=change-me \
  aionforge serve http --listen 127.0.0.1:3918 \
  --bearer-token-env AIONFORGE_MCP_TOKEN
```

`Cargo.lock` pins the exact substrate commit, so builds are reproducible and CI runs
`--locked`. To pull a newer `development` commit, run `cargo update -p selene-core` (and
the other `selene-*` crates).

For tight co-development against a local `selene-db` checkout, uncomment the `[patch]`
block at the bottom of the root `Cargo.toml` and point it at your checkout. Don't commit
the uncommented form.

Set up the shared git hooks once after cloning:

```bash
bash scripts/install-hooks.sh
```

## Documentation

System documentation — subsystem guides and API reference — lives in [`docs/`](docs/).
It grows alongside the build, one subsystem at a time.

## License

Dual-licensed under either [Apache 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), your
choice. Anything you contribute is dual-licensed the same way unless you say
otherwise.
