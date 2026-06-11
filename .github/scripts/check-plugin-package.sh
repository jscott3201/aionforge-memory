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

for file in \
  "$plugin_dir/.codex-plugin/plugin.json" \
  "$plugin_dir/.claude-plugin/plugin.json" \
  "$plugin_dir/.cursor-plugin/plugin.json" \
  "$plugin_dir/plugin.json" \
  "$plugin_dir/.mcp.json" \
  "$plugin_dir/claude.mcp.json" \
  "$plugin_dir/mcp.json" \
  ".agents/plugins/marketplace.json" \
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

for skill in memory-loop memory-recall memory-capture memory-maintenance; do
  metadata="$plugin_dir/skills/$skill/agents/openai.yaml"
  require_file "$metadata"
  [ -f "$metadata" ] || continue
  require_grep "$metadata" 'allow_implicit_invocation: true' "$skill implicit invocation"
  require_grep "$metadata" 'type: "mcp"' "$skill MCP dependency"
  require_grep "$metadata" 'value: "aionforge_memory"' "$skill MCP server name"
done

require_grep "$plugin_dir/.codex-plugin/plugin.json" '"skills": "\./skills/"' "Codex skills path"
require_grep "$plugin_dir/.codex-plugin/plugin.json" '"mcpServers": "\./\.mcp\.json"' "Codex MCP path"
require_grep "$plugin_dir/.claude-plugin/plugin.json" '"mcpServers": "\./claude\.mcp\.json"' "Claude MCP path"
require_grep "$plugin_dir/.cursor-plugin/plugin.json" '"mcpServers": "\./mcp\.json"' "Cursor MCP path"
require_grep "$plugin_dir/plugin.json" '"mcpServers": "mcp\.json"' "Copilot MCP path"

require_grep "$plugin_dir/.mcp.json" '"aionforge_memory"' "Codex MCP server id"
require_grep "$plugin_dir/.mcp.json" '"bearer_token_env_var": "AIONFORGE_MCP_TOKEN"' "Codex bearer token env"
reject_grep "$plugin_dir/.mcp.json" '"Authorization"' "Codex static authorization header"
require_grep "$plugin_dir/claude.mcp.json" '"Authorization": "Bearer \$\{AIONFORGE_MCP_TOKEN\}"' "Claude bearer header"
require_grep "$plugin_dir/mcp.json" '"Authorization": "Bearer \$\{env:AIONFORGE_MCP_TOKEN\}"' "Cursor bearer header"

if grep -RIEq 'sk-[A-Za-z0-9_-]{16,}|Bearer [A-Za-z0-9_-]{16,}' "$plugin_dir" .agents/plugins .cursor-plugin; then
  fail "plugin package appears to contain a literal secret"
fi

if [ "$failures" -gt 0 ]; then
  echo
  echo "Plugin package validation failed with $failures issue(s)."
  exit 1
fi

echo "OK: plugin package manifests, MCP configs, and skills look valid."
