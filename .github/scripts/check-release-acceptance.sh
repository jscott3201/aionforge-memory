#!/usr/bin/env bash
# M8 release-acceptance tripwire.
#
# This is not a replacement for the heavyweight release gate jobs. It is the
# static audit that keeps the 1.0 gate from silently dropping required surfaces:
# docs, production posture, release artifacts, red-team probes, rustls-only bans,
# and explicit deferred-scope statements.

set -euo pipefail

violations=0

fail() {
  echo "FAIL: $*"
  violations=$((violations + 1))
}

require_file() {
  local path="$1"
  if [ ! -f "$path" ]; then
    fail "missing required file: $path"
  fi
}

require_grep() {
  local path="$1"
  local needle="$2"
  local label="$3"
  if ! grep -Fq -- "$needle" "$path"; then
    fail "$label missing from $path (expected: $needle)"
  fi
}

require_file ".github/workflows/release-gate.yml"
require_file ".github/workflows/release.yml"
require_file ".github/workflows/release-publish.yml"
require_file "Dockerfile"
require_file "Dockerfile.release"
require_file "examples/production.toml"
require_file "docs/getting-started.md"
require_file "docs/embedding-guide.md"
require_file "docs/security-model.md"
require_file "docs/honest-scope.md"
require_file "docs/operations-recovery.md"
require_file "docs/mcp-clients.md"
require_file "docs/red-team.md"
require_file "crates/aionforge-redteam/tests/m6t04.rs"
require_file "crates/aionforge-redteam/tests/m6t05.rs"
require_file "crates/aionforge-redteam/tests/m6t06.rs"

# The reusable release gate must remain the single heavy gate shared by the
# main-branch release PR and tag-driven publish workflow.
require_grep ".github/workflows/release.yml" "uses: ./.github/workflows/release-gate.yml" \
  "main release PR reusable gate"
require_grep ".github/workflows/release-publish.yml" "uses: ./.github/workflows/release-gate.yml" \
  "tag publish reusable gate"

# Binding gate coverage.
require_grep ".github/workflows/release-gate.yml" "cargo clippy --workspace --all-targets --all-features --locked -- -D warnings" \
  "workspace clippy gate"
require_grep ".github/workflows/release-gate.yml" "cargo nextest run --workspace --locked --all-features --profile ci" \
  "workspace nextest gate"
require_grep ".github/workflows/release-gate.yml" "cargo test --workspace --locked --all-features --doc" \
  "workspace doctest gate"
require_grep ".github/workflows/release-gate.yml" "rustdoc (no broken intra-doc links)" \
  "rustdoc gate"
require_grep ".github/workflows/release-gate.yml" "M6 red-team probes" \
  "M6 red-team release gate"
require_grep ".github/workflows/release-gate.yml" "cargo deny check bans licenses sources" \
  "cargo-deny release gate"
require_grep ".github/workflows/release-gate.yml" "cargo audit --color always" \
  "cargo-audit release gate"
require_grep ".github/workflows/release-gate.yml" "check-thirdparty-current.sh" \
  "third-party attribution gate"
require_grep ".github/workflows/release-gate.yml" "docker image build + hadolint" \
  "Docker build gate"
require_grep ".github/workflows/release-gate.yml" "docker/setup-buildx-action@v3" \
  "Docker buildx release gate setup"
require_grep ".github/workflows/release-gate.yml" "ubuntu-24.04-arm" \
  "native arm64 Docker release gate runner"
require_grep ".github/workflows/release-gate.yml" "check-release-acceptance.sh" \
  "M8 release acceptance gate"

# Tag publishing must keep all promised artifact families.
require_grep ".github/workflows/release-publish.yml" "linux binary" \
  "Linux binary artifact publishing"
require_grep ".github/workflows/release-publish.yml" "macOS binary" \
  "macOS binary artifact publishing"
require_grep ".github/workflows/release-publish.yml" "container image" \
  "container image publishing"
require_grep ".github/workflows/release-publish.yml" "linux/amd64,linux/arm64" \
  "multi-platform GHCR image publishing"
require_grep ".github/workflows/release-publish.yml" "docker/setup-buildx-action@v3" \
  "multi-platform publish buildx setup"
require_grep ".github/workflows/release-publish.yml" "docker/setup-qemu-action@v3" \
  "multi-platform runtime assembly arm64 emulation setup"
require_grep ".github/workflows/release-publish.yml" "ubuntu-24.04-arm" \
  "native arm64 Linux binary runner"
require_grep ".github/workflows/release-publish.yml" "actions/download-artifact@v4" \
  "GHCR runtime image reuses Linux binary artifacts"
require_grep ".github/workflows/release-publish.yml" "DOCKER_BUILD_RECORD_UPLOAD" \
  "release workflow disables Docker build-record artifacts"
require_grep ".github/workflows/release-publish.yml" "pattern: aionforge-*" \
  "release asset download filters out non-release artifacts"
require_grep ".github/workflows/release-publish.yml" "Dockerfile.release" \
  "release runtime image Dockerfile"
require_grep ".github/workflows/release-publish.yml" "crates.io publishing is intentionally deferred" \
  "crates.io deferral note"
require_grep ".github/workflows/release-publish.yml" "gh release create" \
  "GitHub Release publishing"
require_grep ".github/workflows/release-publish.yml" "--verify-tag" \
  "tag verification before publishing"

# Deployment posture.
require_grep "Dockerfile" "FROM debian:bookworm-slim AS runtime" "source-build Debian runtime image"
require_grep "Dockerfile" "USER 10001:10001" "non-root runtime user"
require_grep "Dockerfile" "chmod 700 /data" "owner-only container data dir"
require_grep "Dockerfile" 'CMD ["serve", "http", "--listen", "0.0.0.0:3918", "--data-dir", "/data"]' \
  "local HTTP default command"
require_grep "Dockerfile.release" "FROM alpine:" "release Alpine runtime image"
require_grep "Dockerfile.release" "USER 10001:10001" "release non-root runtime user"
require_grep "Dockerfile.release" "chmod 700 /data" "release owner-only container data dir"
require_grep "Dockerfile.release" 'CMD ["serve", "http", "--listen", "0.0.0.0:3918", "--data-dir", "/data"]' \
  "release local HTTP default command"
require_grep "examples/production.toml" "signed_writes = true" \
  "production signed-write posture"
require_grep "examples/production.toml" "sign_audit_events = true" \
  "production audit-signing posture"
require_grep "examples/production.toml" "mode = \"refuse\"" \
  "production consolidation guard posture"

# Rustls-only posture is enforced by cargo-deny; keep the bans visible in deny.toml.
require_grep "deny.toml" "{ name = \"native-tls\" }" "native-tls deny rule"
require_grep "deny.toml" "{ name = \"openssl-sys\" }" "openssl-sys deny rule"
require_grep "deny.toml" "{ name = \"openssl-src\" }" "openssl-src deny rule"
require_grep "deny.toml" "{ name = \"openssl\" }" "openssl deny rule"
require_grep "deny.toml" "{ name = \"aws-lc-rs\" }" "aws-lc-rs deny rule"
require_grep "deny.toml" "{ name = \"aws-lc-sys\" }" "aws-lc-sys deny rule"

# Public docs must keep the v1 scope and client surfaces explicit.
require_grep "docs/mcp-clients.md" "## Codex CLI" "Codex MCP client docs"
require_grep "docs/mcp-clients.md" "## Claude Code" "Claude Code MCP client docs"
require_grep "docs/mcp-clients.md" "## OpenCode" "OpenCode MCP client docs"
require_grep "docs/mcp-clients.md" "## Cursor" "Cursor MCP client docs"
require_grep "docs/mcp-clients.md" "## OAuth Readiness" "MCP OAuth docs"
require_grep "docs/security-model.md" "signed_writes = true" "security model production signing"
require_grep "docs/operations-recovery.md" "WAL-backed" "WAL-backed recovery docs"
require_grep "docs/operations-recovery.md" "snapshot publication" "snapshot/WAL boundary docs"
require_grep "docs/honest-scope.md" "M7 is deferred" "M7 deferral docs"
require_grep "docs/honest-scope.md" "LLM-backed consolidation | Not shipped" \
  "deterministic-only consolidation honest scope"
require_grep "docs/honest-scope.md" "cross-family consolidation guard remains" \
  "cross-family guard honest scope"
require_grep "docs/honest-scope.md" "Cost-first routing | Not supported" \
  "no cost-first routing claim"
require_grep "docs/honest-scope.md" "Experiential hand-off | Not shipped" \
  "experiential hand-off deferral"
require_grep "docs/honest-scope.md" "Tagged releases are cut only after human sign-off." \
  "human sign-off release posture"

if [ "$violations" -gt 0 ]; then
  echo
  echo "M8 release acceptance failed with $violations violation(s)."
  exit 1
fi

echo "OK: M8 release acceptance checks passed."
