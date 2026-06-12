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
  "$plugin_dir/claude.mcp.json" \
  "$plugin_dir/cursor.mcp.json" \
  "$plugin_dir/settings.json" \
  ".agents/plugins/marketplace.json" \
  ".claude-plugin/marketplace.json" \
  ".cursor-plugin/marketplace.json"
do
  require_file "$file"
  [ -f "$file" ] && validate_json "$file"
done

require_file "$plugin_dir/README.md"
validate_skill "memory-loop"
validate_skill "memory-recall"
validate_skill "memory-capture"
validate_skill "memory-maintenance"
validate_agent "aionforge-memory-steward"
validate_command "memory-session"
validate_command "memory-handoff"

for skill in memory-loop memory-recall memory-capture memory-maintenance; do
  metadata="$plugin_dir/skills/$skill/agents/openai.yaml"
  require_file "$metadata"
  [ -f "$metadata" ] || continue
  require_grep "$metadata" 'allow_implicit_invocation: true' "$skill implicit invocation"
  require_grep "$metadata" 'type: "mcp"' "$skill MCP dependency"
  require_grep "$metadata" 'value: "aionforge_memory"' "$skill MCP server name"
done

require_grep "$plugin_dir/.codex-plugin/plugin.json" '"skills": "\./skills/"' "Codex skills path"
reject_grep "$plugin_dir/.codex-plugin/plugin.json" '"mcpServers"' "Codex MCP path"
require_grep "$plugin_dir/.claude-plugin/plugin.json" '"mcpServers": "\./claude\.mcp\.json"' "Claude MCP path"
require_grep "$plugin_dir/.cursor-plugin/plugin.json" '"mcpServers": "\./cursor\.mcp\.json"' "Cursor MCP path"
reject_grep "$plugin_dir/plugin.json" '"mcpServers"' "root MCP path"

require_grep "$plugin_dir/settings.json" '"agent": "aionforge-memory-steward"' "Claude default agent setting"
reject_grep "$plugin_dir/claude.mcp.json" '"Authorization"' "Claude static auth header"
reject_grep "$plugin_dir/claude.mcp.json" 'AIONFORGE_MCP_TOKEN' "Claude token env reference"
reject_grep "$plugin_dir/cursor.mcp.json" '"Authorization"' "Cursor static auth header"
reject_grep "$plugin_dir/cursor.mcp.json" 'AIONFORGE_MCP_TOKEN' "Cursor token env reference"

if [ -e "$plugin_dir/.mcp.json" ]; then
  fail "Codex plugin MCP manifest remains at $plugin_dir/.mcp.json"
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
