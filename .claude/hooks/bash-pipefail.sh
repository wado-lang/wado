#!/usr/bin/env bash
# PreToolUse hook for the Bash tool: prepend `set -o pipefail` so that
# pipelines surface non-zero exit codes from upstream commands rather
# than only reporting the last command's status.
#
# Motivation: a bare `mise run X 2>&1 | tail -25` was returning exit 0
# even when the inner task crashed, hiding real failures from the harness
# (see issue #1083). Prepending `set -o pipefail;` is idempotent and has
# no effect on commands without pipes, so it is safe to apply unconditionally.
#
# Each Bash tool invocation runs in a fresh non-interactive shell, so the
# option does not persist across calls — this hook must run every time.

set -euo pipefail

input=$(cat)
cmd=$(printf '%s' "$input" | jq -r '.tool_input.command // empty')

# No command to rewrite (defensive: should not happen for Bash tool input).
if [ -z "$cmd" ]; then
    exit 0
fi

new_cmd="set -o pipefail; $cmd"

jq -n --arg cmd "$new_cmd" '{
  hookSpecificOutput: {
    hookEventName: "PreToolUse",
    updatedInput: { command: $cmd }
  }
}'
