#!/usr/bin/env bash
# Shared body of the Bash PreToolUse guards: deny the command when it matches
# the ERE in $1, with $2 as the explanation.

set -euo pipefail

pattern=$1
reason=$2
cmd=$(jq -r '.tool_input.command // empty')

if [ -n "$cmd" ] && printf '%s' "$cmd" | grep -Eq "$pattern"; then
    jq -n --arg reason "$reason" '{
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        permissionDecision: "deny",
        permissionDecisionReason: $reason
      }
    }'
fi
