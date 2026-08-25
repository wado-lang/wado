#!/usr/bin/env bash
# PreToolUse hook for the Bash tool: block `sed`, `awk`, `python` and `python3`.
#
# `permissions.deny` covers the plain invocations; this covers the forms that
# reach the same interpreters — a pipeline stage, `xargs`, `find -exec`, a
# command substitution, an absolute path such as `/usr/bin/python3`.

set -euo pipefail

input=$(cat)
cmd=$(printf '%s' "$input" | jq -r '.tool_input.command // empty')

if [ -z "$cmd" ]; then
    exit 0
fi

# A command word: at the start, after a shell operator, or after a runner that
# takes a command as its argument. An optional leading path covers /usr/bin/sed.
delimiter='(^|[|&;({]|`|\$\(|[[:space:]](-exec|-execdir|xargs|sudo|env|command|time|nohup|watch)[[:space:]]+)'
forbidden="${delimiter}[[:space:]]*([^[:space:]|&;]*/)?(sed|awk|python3?(\.[0-9]+)?)([[:space:]]|$)"

if printf '%s' "$cmd" | grep -Eq "$forbidden"; then
    jq -n '{
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        permissionDecision: "deny",
        permissionDecisionReason: "sed, awk, python and python3 are forbidden in this repository (AGENTS.md > Tooling). Edit files with the editing tools, one call per change site — an in-place rewrite keeps matching where it was not aimed. Read files with the Read tool or cat/head/tail, search with the Grep tool or grep/rg. For scripting, write Node.js (node, or a .mts script as in .claude/hooks/)."
      }
    }'
    exit 0
fi

exit 0
