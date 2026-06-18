<!--
  Keep this PR tight. Fill every section. Guidance lives in HTML comments so the
  rendered PR stays clean. Authoritative gate commands + core invariants live in
  AGENTS.md — this template points to them rather than restating them.
-->

## Summary

<!-- What changed and WHY, in a few bullets. Link any RFC under _ideas/ if relevant. -->

-

## Linked issue

<!-- "Closes #N" auto-closes the issue on merge. Use "Refs #N" if it only relates. -->

Closes #

## Type of change

- [ ] Bug fix
- [ ] Feature / enhancement
- [ ] Documentation
- [ ] Chore (deps, tooling, CI, refactor)
- [ ] Design / RFC follow-up

## Validation

<!--
  Tick the gates you actually ran locally. These mirror ci.yml exactly and are
  copy-pasteable. See AGENTS.md for the canonical command list. Install hooks once:
  bash scripts/install-hooks.sh
-->

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
- [ ] `cargo nextest run --workspace --locked --all-features --profile ci`
- [ ] `cargo test --workspace --locked --all-features --doc`
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --lib --document-private-items --locked`
- [ ] Repository fast gates: `bash .github/scripts/check-file-size.sh`, `check-no-secrets.sh`, `check-plugin-package.sh`, `check-no-gql-interpolation.sh`, `check-store-only-selene.sh`, `check-audit-keygen-confined.sh`, `check-principal-gate.sh`
- [ ] Console changes ran: `cd ui/console && pnpm install --frozen-lockfile && pnpm validate` — or N/A

<!--
  Dependency / manifest changes only (Cargo.lock, Cargo.toml, crates/*/Cargo.toml,
  deny.toml, about.hbs, about.toml). These run in CI only when those paths change.
-->

- [ ] Dependency change ran: `cargo deny check bans licenses sources` and `bash .github/scripts/check-thirdparty-current.sh` — or N/A

<!--
  Note: the heavy ubuntu + macOS clippy/test matrix runs only at the
  development -> main release gate, not on development PRs.
-->

## Scope & invariants

<!-- Core invariants are defined in AGENTS.md. Confirm this change respects them. -->

- [ ] Change is subsystem-scoped and respects the layered acyclic boundaries (selene-db types stay inside `aionforge-store`; no raw GQL interpolation; no `unsafe`; public APIs have doc comments; every source file under the 700-LOC cap). See AGENTS.md.

## Public-repo check

- [ ] This PR body describes code, behavior, and validation only. It does not include private planning notes, internal handoff text, agent transcripts, secrets, or customer data.
