#!/usr/bin/env bash
# Validate the public plugin package enough to catch broken manifests and skill
# frontmatter before a PR merges.

set -euo pipefail

plugin_dir="plugins/aionforge-memory"
failures=0

fail() {
  echo "FAIL: $1"
  failures=$((failures + 1))
}

require_file() {
  if [ ! -f "$1" ]; then
    fail "missing required file: $1"
  fi
}

require_grep() {
  file="$1"
  pattern="$2"
  label="$3"
  if ! grep -Eq "$pattern" "$file"; then
    fail "$label missing from $file"
  fi
}

reject_grep() {
  file="$1"
  pattern="$2"
  label="$3"
  if grep -Eq "$pattern" "$file"; then
    fail "$label found in $file"
  fi
}

validate_json() {
  file="$1"
  if ! python3 -m json.tool "$file" >/dev/null; then
    fail "invalid JSON: $file"
  fi
}

validate_toml() {
  file="$1"
  if ! python3 - "$file" <<'PY'
import pathlib
import sys
import tomllib

tomllib.loads(pathlib.Path(sys.argv[1]).read_text())
PY
  then
    fail "invalid TOML: $file"
  fi
}

validate_skill() {
  name="$1"
  file="$plugin_dir/skills/$name/SKILL.md"
  require_file "$file"
  [ -f "$file" ] || return

  first_line="$(sed -n '1p' "$file")"
  if [ "$first_line" != "---" ]; then
    fail "$file must start with YAML frontmatter"
  fi

  require_grep "$file" "^name: $name$" "skill name $name"
  require_grep "$file" "^description: .+" "skill description"
  require_grep "$file" "^license: MIT OR Apache-2.0$" "skill license"
}

validate_command() {
  name="$1"
  file="$plugin_dir/commands/$name.md"
  require_file "$file"
  [ -f "$file" ] || return

  first_line="$(sed -n '1p' "$file")"
  if [ "$first_line" != "---" ]; then
    fail "$file must start with YAML frontmatter"
  fi

  require_grep "$file" "^description: .+" "command description $name"
  require_grep "$file" "^argument-hint: .+" "command argument hint $name"
}

validate_agent() {
  name="$1"
  file="$plugin_dir/agents/$name.md"
  require_file "$file"
  [ -f "$file" ] || return

  first_line="$(sed -n '1p' "$file")"
  if [ "$first_line" != "---" ]; then
    fail "$file must start with YAML frontmatter"
  fi

  require_grep "$file" "^name: $name$" "agent name $name"
  require_grep "$file" "^description: .+" "agent description $name"
  require_grep "$file" "^model: .+" "agent model $name"
  require_grep "$file" "^color: .+" "agent color $name"
}

for file in \
  "$plugin_dir/.codex-plugin/plugin.json" \
  "$plugin_dir/.claude-plugin/plugin.json" \
  "$plugin_dir/.cursor-plugin/plugin.json" \
  "$plugin_dir/plugin.json" \
  "$plugin_dir/settings.json" \
  "$plugin_dir/hooks/hooks.json" \
  ".agents/plugins/marketplace.json" \
  ".claude-plugin/marketplace.json" \
  ".cursor-plugin/marketplace.json"
do
  require_file "$file"
  [ -f "$file" ] && validate_json "$file"
done

require_file "$plugin_dir/README.md"
require_file "$plugin_dir/NUDGE.md"
# Keep the published plugin reference doc from silently dropping a packaged skill.
require_grep "docs/plugins.md" 'work-tracking' "docs/plugins.md work-tracking skill"
require_grep "docs/plugins.md" 'memory-bootstrap' "docs/plugins.md memory-bootstrap skill"
validate_skill "memory-loop"
validate_skill "memory-recall"
validate_skill "memory-capture"
validate_skill "work-tracking"
validate_skill "memory-maintenance"
validate_skill "memory-bootstrap"
validate_agent "aionforge-memory-steward"
validate_command "memory-session"
validate_command "memory-handoff"
validate_command "memory-bootstrap"

for skill in memory-loop memory-recall memory-capture work-tracking memory-maintenance memory-bootstrap; do
  metadata="$plugin_dir/skills/$skill/agents/openai.yaml"
  require_file "$metadata"
  [ -f "$metadata" ] || continue
  require_grep "$metadata" 'allow_implicit_invocation: true' "$skill implicit invocation"
  require_grep "$metadata" 'type: "mcp"' "$skill MCP dependency"
  require_grep "$metadata" 'value: "aionforge_memory"' "$skill MCP server name"
done

require_grep "$plugin_dir/.codex-plugin/plugin.json" '"skills": "\./skills/"' "Codex skills path"
reject_grep "$plugin_dir/.codex-plugin/plugin.json" '"mcpServers"' "Codex MCP path"
reject_grep "$plugin_dir/.claude-plugin/plugin.json" '"mcpServers"' "Claude MCP path"
reject_grep "$plugin_dir/.cursor-plugin/plugin.json" '"mcpServers"' "Cursor MCP path"
reject_grep "$plugin_dir/plugin.json" '"mcpServers"' "root MCP path"
reject_grep ".cursor-plugin/marketplace.json" '"mcpServers"' "Cursor marketplace MCP path"

require_grep "$plugin_dir/settings.json" '"agent": "aionforge-memory-steward"' "Claude default agent setting"

# SessionStart nudge hook. Wired in hooks/hooks.json to a bundled, executable script.
# PreCompact is intentionally NOT used (it is blocking-only and cannot inject context).
hooks_json="$plugin_dir/hooks/hooks.json"
nudge_script="$plugin_dir/hooks/session-start-nudge.sh"
require_grep "$hooks_json" '"SessionStart"' "SessionStart hook event"
require_grep "$hooks_json" 'CLAUDE_PLUGIN_ROOT' "hook plugin-root command reference"
require_grep "$hooks_json" 'session-start-nudge\.sh' "hook script reference"
reject_grep "$hooks_json" '"mcpServers"' "hooks MCP path"
reject_grep "$hooks_json" '"PreCompact"' "PreCompact hook (blocking-only, cannot inject context)"
require_file "$nudge_script"
if [ -f "$nudge_script" ] && [ ! -x "$nudge_script" ]; then
  fail "hook script is not executable: $nudge_script"
fi

# Cursor always-apply nudge rule, bundled and registered on install. It must be an
# always-on rule (alwaysApply: true), declared by the Cursor manifest's rules field,
# and carry no MCP config.
cursor_rule="$plugin_dir/rules/aionforge-memory.mdc"
require_file "$cursor_rule"
require_grep "$cursor_rule" '^alwaysApply: true$' "Cursor always-apply rule type"
# The .mdc is YAML+markdown, so guard the bare YAML `mcpServers:` key too, not just the
# JSON-quoted token a manifest would use.
reject_grep "$cursor_rule" '("mcpServers"|^[[:space:]]*mcpServers:)' "Cursor rule MCP path"
# Anchor to the key AND value Cursor reads, mirroring the Codex skills-path check, so a
# dropped/repointed field cannot pass on an incidental "rules" substring.
require_grep "$plugin_dir/.cursor-plugin/plugin.json" '"rules"[[:space:]]*:[[:space:]]*"\./rules/"' "Cursor rules field"

# Landscape wiring guide for the editors the plugin cannot auto-bundle.
require_file "docs/agent-nudges.md"
require_grep "docs/agent-nudges.md" 'OpenCode' "agent-nudges OpenCode coverage"

# Vocabulary lock: every surface that nudges must route tasks to work items, so
# work_create has to appear in the canonical source, the skill, the steward agent,
# the Codex default prompt, the Cursor rule, and the landscape guide. This catches a
# surface drifting off the shared lock.
for surface in \
  "$plugin_dir/NUDGE.md" \
  "$plugin_dir/skills/work-tracking/SKILL.md" \
  "$plugin_dir/agents/aionforge-memory-steward.md" \
  "$plugin_dir/.codex-plugin/plugin.json" \
  "$cursor_rule" \
  "docs/agent-nudges.md"
do
  require_grep "$surface" 'work_create' "work-item routing"
done

# The two new always-on surfaces (Cursor rule + landscape guide) are short distillations
# with no SKILL.md body behind them, so lock the routing prose itself: the capture-vs-work
# split and the notes-are-consolidation-derived rule must both be present.
for surface in "$cursor_rule" "docs/agent-nudges.md"; do
  require_grep "$surface" 'consolidate' "consolidate-derives-notes lock"
  require_grep "$surface" 'no "note" to store directly|notes are derived' "no-direct-note routing lock"
done

if [ -e "$plugin_dir/.mcp.json" ]; then
  fail "Codex plugin MCP manifest remains at $plugin_dir/.mcp.json"
fi

if [ -e "$plugin_dir/claude.mcp.json" ]; then
  fail "Claude plugin MCP manifest remains at $plugin_dir/claude.mcp.json"
fi

if [ -e "$plugin_dir/cursor.mcp.json" ]; then
  fail "Cursor plugin MCP manifest remains at $plugin_dir/cursor.mcp.json"
fi

if [ -e "$plugin_dir/codex.plugin-policy.example.toml" ]; then
  fail "Codex plugin-scoped MCP policy remains at $plugin_dir/codex.plugin-policy.example.toml"
fi

if [ -e "$plugin_dir/mcp.json" ]; then
  fail "legacy generic MCP manifest remains at $plugin_dir/mcp.json"
fi

if grep -RIEq 'sk-[A-Za-z0-9_-]{16,}|Bearer [A-Za-z0-9_-]{16,}' "$plugin_dir" .agents/plugins .claude-plugin .cursor-plugin; then
  fail "plugin package appears to contain a literal secret"
fi

if [ "$failures" -gt 0 ]; then
  echo
  echo "Plugin package validation failed with $failures issue(s)."
  exit 1
fi

echo "OK: plugin package manifests, MCP configs, and skills look valid."
