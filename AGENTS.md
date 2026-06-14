# Agent Guide for Aionforge Memory

Aionforge Memory is a Rust long-term memory layer for AI agents. It stores
episodes, facts, notes, skills, bad patterns, core memory, and audit events in
`selene-db`, then retrieves relevant context with lexical anchors, vector search,
graph traversal, recency, importance, and trust signals.

Use this file as a codebase navigation guide. Keep private process notes,
secrets, transcripts, and local-only preferences out of this repository.

## Start Here

- `README.md` explains the public product scope, installation paths, MCP server,
  Docker usage, and Rust library entry point.
- `docs/README.md` is the documentation map for subsystem behavior.
- `Cargo.toml` is the workspace map and contains the intended crate layering.
- `.github/workflows/ci.yml` is the source of truth for PR validation.
- `CONTRIBUTING.md` is the human onboarding guide: environment setup, the
  branch/release model, the commit convention, and the pre-PR gate block.
- `.github/pull_request_template.md` and `.github/ISSUE_TEMPLATE/` carry the PR
  and issue (bug, feature, RFC/design) forms.

Local scratch or planning directories may exist in some checkouts under `_*/`.
They are not part of the public codebase contract. Prefer the tracked README,
`docs/`, `crates/`, scripts, and CI configuration when deciding how the project
works.

## Workspace Layout

The workspace is intentionally layered and acyclic:

- `crates/aionforge-domain`: memory-kind types and contract traits; no I/O.
- `crates/aionforge-store`: `selene-db` storage adapter, shared graph, and
  persistence. This is the only crate that should name `selene-db` types.
- `crates/aionforge-config`: layered file, environment, and flag configuration,
  including the `[auth]` (OAuth resource-server posture) and `[server]`
  (Streamable HTTP bind/allow-list) blocks.
- `crates/aionforge-auth`: OIDC/JWKS discovery and an RS256-pinned JWT validator
  (OAuth resource-server support; default-off).
- `crates/aionforge-embed`: OpenAI-compatible embedding and rerank client.
- `crates/aionforge-chat`: multi-provider chat-completion client for optional
  LLM-backed layers, kept off the deterministic canonical path.
- `crates/aionforge-capture`: fast capture path, redaction, injection filtering,
  deduplication, embedding, and provenance recording.
- `crates/aionforge-retrieval`: hybrid recall, router, rank fusion, graph
  signals, and rendered recall bundles.
- `crates/aionforge-consolidate`: deterministic background consolidation,
  entity/fact extraction, supersession, contradiction quarantine, and notes.
- `crates/aionforge-procedural`: skills-as-data and bad-pattern memory.
- `crates/aionforge-trust`: namespaces, provenance, signed writes, audit
  signing, trust, CRDT merge, and promotion support.
- `crates/aionforge-forget`: decay, soft forgetting, erasure, identity, and
  drift support.
- `crates/aionforge-security`: privacy filters, untrusted recall tagging,
  cross-family guards, and security helpers.
- `crates/aionforge-redteam`: security acceptance probes and structured reports.
- `crates/aionforge-engine`: facade that composes subsystems and enforces
  cross-cutting policy.
- `crates/aionforge`: public Rust library API.
- `crates/aionforge-mcp`: MCP server surface.
- `crates/aionforge-tui`: read-only operator TUI. Slated for retirement in favor
  of an operator console; do not invest in new TUI features.
- `crates/aionforge-cli`: the `aionforge` binary.
- `plugins/aionforge-memory`: agent plugin package and client-facing assets.

## Core Invariants

- Preserve the crate dependency direction. Domain and subsystem crates must not
  depend directly on `selene-db`; route storage access through
  `aionforge-store`.
- Keep deterministic paths deterministic. Capture, consolidation, and retrieval
  should produce the same canonical result for the same inputs and graph state.
  Optional LLM-backed layers must stay outside that canonical path.
- Treat recalled memory as untrusted third-party data. MCP recall output is
  wrapped in a `<recalled-memory-context>` envelope and must not become
  executable instruction text.
- Keep namespace boundaries explicit. Agent-private, team, global, and system
  memory are separate policy surfaces.
- Favor typed APIs and structured parsing over ad hoc string construction,
  especially for graph queries, wire formats, configuration, and audit records.
- Preserve the security gates: no raw GQL interpolation, no unconstrained audit
  key generation, no secret-bearing fixtures, and no `selene-db` types outside
  `aionforge-store`.
- Public APIs require documentation. The workspace denies missing docs and
  forbids unsafe code.
- The repository tracks Rust 1.95.0 and edition 2024 in `rust-toolchain.toml`.

## Running Locally

Install the git hooks once after cloning:

```bash
bash scripts/install-hooks.sh
```

Build and test the full workspace:

```bash
cargo build --workspace --locked
cargo nextest run --workspace --locked --all-features
cargo test --workspace --locked --all-features --doc
```

Run the MCP server over stdio:

```bash
aionforge serve stdio
```

Run the local Streamable HTTP server on loopback only:

```bash
aionforge serve http --listen 127.0.0.1:3918
```

Do not expose the built-in HTTP server to a shared network without an external
OAuth-aware resource-server verifier or equivalent perimeter.

## Validation

For code changes, run the relevant local checks before opening a PR:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo nextest run --workspace --locked --all-features --profile ci
cargo test --workspace --locked --all-features --doc
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --lib --document-private-items --locked
```

Fast repository gates:

```bash
bash .github/scripts/check-file-size.sh
bash .github/scripts/check-no-secrets.sh
bash .github/scripts/check-plugin-package.sh
bash .github/scripts/check-no-gql-interpolation.sh
bash .github/scripts/check-store-only-selene.sh
bash .github/scripts/check-audit-keygen-confined.sh
bash .github/scripts/check-principal-gate.sh
```

Dependency or manifest changes also require:

```bash
cargo deny check bans licenses sources
bash .github/scripts/check-thirdparty-current.sh
```

The CI workflow runs on pull requests into `development`. Doc-only changes still
run formatting and repository safety gates; the heavier Rust build, lint, test,
and documentation matrix runs at the `development` -> `main` release gate, not on
every development PR. See `CONTRIBUTING.md` for the full branch/release model.

## Editing Guidance

- Keep changes scoped to the crate or subsystem that owns the behavior.
- Add or update tests when changing behavior, persistence, retrieval ranking,
  authorization, audit output, wire formats, or error handling.
- Prefer existing subsystem patterns over introducing new abstractions.
- Keep files under the CI file-size cap enforced by
  `.github/scripts/check-file-size.sh`.
- Do not commit local-only config, generated build output, secrets, private
  planning notes, or agent transcripts.
- When modifying MCP behavior, update tests and public docs so tool semantics,
  safety posture, and client-facing resources remain aligned.
