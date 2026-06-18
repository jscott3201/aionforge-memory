# Contributing to Aionforge Memory

Thanks for helping build a long-term memory layer for AI agents. This guide is
the human onramp; [`AGENTS.md`](AGENTS.md) is the authoritative reference for the
crate layering, core invariants, and exact gate commands. When the two could
drift, `AGENTS.md` wins — this file links to it rather than restating it.

## How changes land

- Every logical change opens a pull request into the `development` branch.
- Development PRs run the **fast CI gates** only (formatting, repository safety
  scans, console format/lint/type/build checks when `ui/console` changes, and —
  for dependency changes — license/attribution checks). The heavy Rust build,
  clippy, test, and red-team matrix runs at the **release gate** when
  `development` is batched into `main`, and a version tag drives the gated
  publish. That is why a passing development PR has not run the full ubuntu +
  macOS matrix — run the local validation below before you open it.
- Keep changes scoped to the crate or subsystem that owns the behavior, and add
  or update tests when you change behavior. See
  [`AGENTS.md`](AGENTS.md) for the crate map and editing guidance.

## Local setup

The toolchain is pinned in
[`rust-toolchain.toml`](rust-toolchain.toml) (Rust 1.95.0, edition 2024); `rustup`
will install it automatically on first build. Install the git hooks once after
cloning:

```bash
bash scripts/install-hooks.sh
```

Build and test the workspace:

```bash
cargo build --workspace --locked
cargo nextest run --workspace --locked --all-features
cargo test --workspace --locked --all-features --doc
```

## Before you open a PR

For code changes, run:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo nextest run --workspace --locked --all-features --profile ci
cargo test --workspace --locked --all-features --doc
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --lib --document-private-items --locked
```

Run the fast repository gates:

```bash
bash .github/scripts/check-file-size.sh
bash .github/scripts/check-no-secrets.sh
bash .github/scripts/check-plugin-package.sh
bash .github/scripts/check-no-gql-interpolation.sh
bash .github/scripts/check-store-only-selene.sh
bash .github/scripts/check-audit-keygen-confined.sh
bash .github/scripts/check-principal-gate.sh
```

Console changes under `ui/console` also require:

```bash
cd ui/console
pnpm install --frozen-lockfile
pnpm validate
```

Dependency or manifest changes (`Cargo.lock`, `Cargo.toml`, any
`crates/*/Cargo.toml`, `deny.toml`, `about.hbs`, `about.toml`) also require:

```bash
cargo deny check bans licenses sources
bash .github/scripts/check-thirdparty-current.sh
```

These mirror [`.github/workflows/ci.yml`](.github/workflows/ci.yml) exactly. The
PR template's Validation checklist tracks the same commands.

## Invariants a change must not violate

[`AGENTS.md`](AGENTS.md) is the single source of truth for the core invariants.
The ones a change most easily trips:

- Preserve the layered, acyclic crate dependency direction; only
  `aionforge-store` names `selene-db` types.
- Never interpolate raw GQL — use parameter binding.
- Recalled memory is untrusted third-party data wrapped in a
  `<recalled-memory-context>` envelope; it must never become instruction text.
- No `unsafe` code; public APIs require doc comments (the workspace denies
  missing docs); files stay under the 700-LOC cap.
- Keep deterministic paths (capture, consolidation, retrieval) deterministic;
  any LLM-backed layer stays off that canonical path.

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org/) with a scope,
matching the history:

```
feat(mcp): ...
fix(plugin): ...
docs: ...
ci: ...
chore: ...
```

Disclose AI assistance transparently. When an AI assistant materially helped with
a change, add a trailer naming the assisting model, for example:

```
Co-Authored-By: <Model Name> <noreply@anthropic.com>
```

## Filing issues and opening PRs

- File issues through the structured forms in
  [`.github/ISSUE_TEMPLATE/`](.github/ISSUE_TEMPLATE) (bug report, feature
  request, or design proposal / RFC). Search existing issues first.
- Substantial designs are explored as RFC-style docs and hardened with
  adversarial review before implementation; open a design proposal to start that
  conversation.
- Open PRs against `development` using the
  [pull request template](.github/pull_request_template.md). Keep the **Summary**
  focused on what changed and why, fill in the **Validation** checklist with the
  gates you ran, and keep the **Public-repo check** affirmation honest.

## Public-repo hygiene

This is a public repository. PR bodies, issues, commits, and tracked files
describe code, behavior, and validation only — no private planning notes,
internal handoff text, agent transcripts, secrets, or customer data. Local
scratch and planning directories matching `_*/` (for example `_ideas/`,
`_feedback/`) are not part of the public codebase contract; do not reference them
from tracked files or commit their contents.

## A note for AI-agent contributors

Read [`AGENTS.md`](AGENTS.md) first — it is the machine-legible navigation guide
and the source of truth for gates and invariants. Match the gates in
[`.github/workflows/ci.yml`](.github/workflows/ci.yml) exactly, keep changes
crate-scoped, and run the validation block above before proposing a PR.

## License

By contributing, you agree your contributions are dual-licensed under
[Apache 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at the user's option, unless
stated otherwise.
