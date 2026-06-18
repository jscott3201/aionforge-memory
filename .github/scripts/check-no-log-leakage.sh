#!/usr/bin/env bash
# No-log-leakage gate (logging-foundation, task #9; see docs/observability.md).
#
# This is a MEMORY STORE: a log line that interpolates memory content, embeddings,
# tokens, or keys is a data leak. The rule is "log identifiers, kinds, counts,
# latencies, and outcomes — never the sensitive payload itself." This gate is a
# heuristic tripwire, not a parser: it flags a `tracing`/`log` macro call that, on the
# same line, reaches into a sensitive field by dot-access (e.g. `%memory.content`,
# `x.statement`, `claims.token`). Real safety rests on code review + the convention.
#
# Escape hatch: append `// log-leak-ok` to a line that is genuinely safe (e.g. logging
# a hash or length of one of these fields). For `.content`/`.embedding` there should be
# none — log the id or the byte count instead.
#
# Known limits (documented, not bugs — this is a tripwire, not a parser):
#   1. Line-based: a sensitive field split onto its own line inside a multi-line macro can
#      evade it. Keep one field per macro line.
#   2. Indirection: a field pre-formatted into a string on one line and logged on another
#      (`let m = format!("{}", x.secret); info!("{m}");`) is not caught — the macro line has
#      no dot-access and the format! line has no macro. Don't pre-format sensitive fields into
#      log strings; use structured `tracing` fields directly. (A format! check is deliberately
#      omitted: format! is also used for hashing/dedup of content, where it is legitimate.)
# Real safety rests on these conventions + code review; the gate catches the common footgun.
#
# Runs from repo root. macOS bash 3.x compatible.

set -euo pipefail

# Logging macros: tracing event + *_span forms and the `log` crate, optionally path-qualified.
MACRO='(tracing::|log::)?(trace|debug|info|warn|error)(_span)?!'
# Sensitive fields, matched only as a dot-access (`.content`) with a trailing word boundary, so
# `content_hash`, `token_limit`, `est_tokens`, and `content_type` do NOT false-positive.
FIELDS='content|statement|embedding|api_key|secret|private_key|password|bearer|token'
ALLOW='log-leak-ok'
violations=0

check() {
  local pattern="$1"
  local label="$2"
  local hits
  hits=$(git grep -nE "$pattern" -- '*.rs' 2>/dev/null | grep -vE "$ALLOW" || true)
  if [ -n "$hits" ]; then
    echo "FAIL: possible sensitive-field leak into a log/tracing macro ($label):"
    echo "$hits"
    echo
    violations=$((violations + 1))
  fi
}

# A logging macro that, on the same line, dot-accesses a sensitive field.
check "${MACRO}[[:space:]]*\\(.*\\.(${FIELDS})\\b" \
  "sensitive field reached by a logging macro"

if [ "$violations" -gt 0 ]; then
  echo "Log an id, kind, count, length, or hash — never memory content, embeddings, tokens, or keys."
  echo "If the flagged line is genuinely safe (e.g. a hash or length), add // log-leak-ok with a reason."
  exit 1
fi

echo "OK: no log-leakage patterns found."
