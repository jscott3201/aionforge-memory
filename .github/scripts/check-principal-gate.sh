#!/usr/bin/env bash
# Security-invariant gate (OAuth workstream PR4): the MCP identity resolvers are the
# ONLY sites that mint a request-path `Principal`, and every tool that consumes a
# validated identity routes it THROUGH a resolver. Two halves, both enforced:
#
#   NEGATIVE — no tool handler hand-rolls a `Principal`. A direct construction would
#   bypass the auth precedence / reject-on-absent / read-only write-guard rules and,
#   worse, could forge the server-only `operator` capability. Forbidden in every file
#   under crates/aionforge-mcp/src/ except the two mint sites below:
#     * principal.rs — resolve_reader/resolve_writer (the request-path mint site).
#     * mapper.rs    — the validated-claims→Principal mapper (the token mint site, the
#                      ONLY caller of Principal::with_operator).
#   Both the associated-function calls (`Principal::new|agent|with_operator(`) AND the
#   struct-literal form (`Principal { ... }`, which can set `operator: true` directly —
#   all of `Principal`'s fields are pub) are forbidden. `Principal`'s fields being public
#   means a plain literal is a real, operator-forging bypass, so the literal MUST be
#   caught, not just the constructors.
#
#   POSITIVE — every tool function that ACCEPTS a validated extension
#   (`extension: Option<ValidatedPrincipal>`) must route it through a resolver
#   (`resolve_reader`, `resolve_writer`, or the shared `refuse_read_only_write` guard) in
#   its own body. A new tool that reads the extension and uses `.principal` directly, or
#   serves data with no identity resolution, fails here. This is the "resolvers are the
#   only mint sites" invariant's other half.
#
# Test code is out of scope: properly-delimited `#[cfg(test)] mod ... { ... }` blocks may
# build principals for fixtures. They are stripped with a brace-counter (NOT a blanket
# truncate-at-first-cfg(test)), so an inline `#[cfg(test)] use ...;` or a mid-file test
# module never blinds the scan of the production code that follows it.
#
# Files are enumerated via a filesystem walk (`find`), NOT `git ls-files`, so a
# newly-added-but-unstaged handler file is still covered. Runs from repo root.
# macOS bash 3.x compatible.

set -euo pipefail

SRC_DIR="crates/aionforge-mcp/src"

# A `Principal` mint: either an associated-function call or a struct literal. The literal
# arm uses a negative lookbehind-free guard: `(^|[^A-Za-z0-9_])` ensures the match is the
# bare `Principal` type, never a suffix of `ValidatedPrincipal`/`HostPrincipalToolParam`.
CTOR_PATTERN='(^|[^A-Za-z0-9_])Principal::(new|agent|with_operator)[[:space:]]*\('
LITERAL_PATTERN='(^|[^A-Za-z0-9_])Principal[[:space:]]*\{'

# Strip properly-delimited `#[cfg(test)] mod ... { ... }` blocks (brace-counted), printing
# only production lines, each prefixed with its ORIGINAL line number (`NR:line`) so a
# violation report points at the real location. When not inside a test module we watch for a
# `#[cfg(test)]` attribute; the next `mod` it gates opens a block we skip until its matching
# close brace. Any `Principal` mint outside such a block survives. Uses only POSIX awk regex
# (no `\<`/`\>` word boundaries — macOS awk lacks them); `mod` is matched with explicit
# non-identifier boundaries so it never trips on `module`/`my_mod`.
strip_test_modules() {
  awk '
    BEGIN { pending_cfg = 0; depth = 0; in_test = 0 }
    {
      if (in_test) {
        n = gsub(/\{/, "{"); m = gsub(/\}/, "}")
        depth += n - m
        if (depth <= 0) { in_test = 0; depth = 0 }
        next
      }
      if ($0 ~ /#\[cfg\(test\)\]/) { pending_cfg = 1; next }
      if (pending_cfg && $0 ~ /(^|[^A-Za-z0-9_])mod([^A-Za-z0-9_]|$)/) {
        pending_cfg = 0
        n = gsub(/\{/, "{"); m = gsub(/\}/, "}")
        depth = n - m
        if (depth > 0) { in_test = 1 }   # block opened on this line; skip until it closes
        else if ($0 ~ /\{/) { depth = 0 } # one-line `mod m {}` — fully consumed
        else { in_test = 1; depth = 1 }   # `mod tests` with the brace on a later line
        next
      }
      # A `#[cfg(test)]` not gating a `mod` (a test-only `use`/`fn`/`const`): the attribute
      # line itself is dropped; this line is production and is printed normally.
      pending_cfg = 0
      printf "%d:%s\n", NR, $0
    }
  ' "$1"
}

violations=0

# Collect the production-source files via a filesystem walk so unstaged files are scanned.
while IFS= read -r f; do
  base="${f##*/}"
  # The two allowed mint sites are exempt from the NEGATIVE check only.
  case "$base" in
    principal.rs | mapper.rs) skip_negative=1 ;;
    *) skip_negative=0 ;;
  esac

  production=$(strip_test_modules "$f")

  if [ "$skip_negative" -eq 0 ]; then
    # `production` lines are already prefixed `NR:` by the stripper, so grep without -n.
    hits=$(printf '%s\n' "$production" | grep -E "$CTOR_PATTERN" || true)
    lits=$(printf '%s\n' "$production" | grep -E "$LITERAL_PATTERN" || true)
    if [ -n "$hits" ] || [ -n "$lits" ]; then
      echo "FAIL: $f constructs a Principal directly (route identity through resolve_reader/resolve_writer):"
      [ -n "$hits" ] && printf '%s\n' "$hits"
      [ -n "$lits" ] && printf '%s\n' "$lits"
      violations=$((violations + 1))
    fi
  fi

  # POSITIVE check (every file): a tool fn that takes a validated extension must route it
  # through a resolver in the same file. We approximate "takes a validated extension" by the
  # parameter declaration; the resolver call must appear in the same production text.
  if grep -qE 'extension:[[:space:]]*Option<[[:space:]]*ValidatedPrincipal' <<< "$production"; then
    if ! grep -qE 'resolve_reader|resolve_writer|refuse_read_only_write' <<< "$production"; then
      echo "FAIL: $f accepts a validated extension but never routes it through a resolver"
      echo "      (resolve_reader/resolve_writer/refuse_read_only_write). Identity must be minted by the resolvers."
      violations=$((violations + 1))
    fi
  fi
done < <(find "$SRC_DIR" -name '*.rs' -type f | sort)

if [ "$violations" -gt 0 ]; then
  echo
  echo "Only principal.rs (the resolvers) and mapper.rs (the claims mapper) may mint a Principal,"
  echo "and every tool that consumes a validated extension must route it through a resolver (OAuth PR4)."
  exit 1
fi

echo "OK: every MCP request-path identity is minted through the resolvers, and every"
echo "    validated-extension consumer routes through resolve_reader/resolve_writer."
