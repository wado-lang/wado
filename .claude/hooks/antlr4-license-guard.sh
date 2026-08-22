#!/usr/bin/env bash
# PreToolUse hook for the Bash tool: enforce the License hygiene rule in
# `package-gale/AGENTS.md`. `permissions.deny` covers the Read tool; this covers
# `cat`, `grep`, `sed`, `find -exec` and friends, which reach the same bytes.

set -euo pipefail

input=$(cat)
cmd=$(printf '%s' "$input" | jq -r '.tool_input.command // empty')

if [ -z "$cmd" ]; then
    exit 0
fi

# Anchored on `vendor/antlr4/` so a `runtime/` elsewhere in the repo is untouched.
forbidden='vendor/antlr4/(tool/[^[:space:]]*\.(java|g)|runtime/[^[:space:]]*\.java)'

if printf '%s' "$cmd" | grep -Eq "$forbidden"; then
    jq -n '{
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        permissionDecision: "deny",
        permissionDecisionReason: "License hygiene (package-gale/AGENTS.md): ANTLR4 implementation source under vendor/antlr4/{tool,runtime} must not be read. To settle what ANTLR does, run the published jar as a black box (package-gale/scripts/antlr4-oracle.sh) or read vendor/antlr4/doc/*.md. Reading .g4 files and runtime-testsuite descriptors is allowed. Naming such a path in a command trips this too, even when not reading it — use the Edit tool."
      }
    }'
    exit 0
fi

exit 0
