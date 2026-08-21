#!/usr/bin/env bash
# PreToolUse hook for the Bash tool: refuse commands that would read ANTLR4
# implementation source under vendor/antlr4/.
#
# `package-gale/AGENTS.md` ("License hygiene") forbids reading
# `vendor/antlr4/tool/**/*.{java,g}` and `vendor/antlr4/runtime/**/*.java`:
# ANTLR4 is BSD-3, so paraphrasing its implementation risks making Gale a
# derivative work. Reading `.g4` files, the `runtime-testsuite` descriptors and
# `vendor/antlr4/doc/*.md` stays allowed, as does running the published jar as
# a black-box oracle.
#
# The permissions.deny rules cover the Read tool. This covers the other half:
# `cat`, `sed`, `grep`, `head`, `less`, `find -exec` and friends reach the same
# bytes and would otherwise walk straight past the rule.

set -euo pipefail

input=$(cat)
cmd=$(printf '%s' "$input" | jq -r '.tool_input.command // empty')

if [ -z "$cmd" ]; then
    exit 0
fi

# A path under the two forbidden trees, with a forbidden extension. Anchored on
# `vendor/antlr4/` so an unrelated `runtime/` elsewhere in the repo is untouched.
forbidden='vendor/antlr4/(tool/[^[:space:]]*\.(java|g)|runtime/[^[:space:]]*\.java)'

if printf '%s' "$cmd" | grep -Eq "$forbidden"; then
    jq -n '{
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        permissionDecision: "deny",
        permissionDecisionReason: "License hygiene (package-gale/AGENTS.md): ANTLR4 implementation source under vendor/antlr4/{tool,runtime} must not be read. To settle what ANTLR does, run the published jar as a black box (package-gale/scripts/antlr4-oracle.sh) or read vendor/antlr4/doc/*.md. Reading .g4 files and runtime-testsuite descriptors is allowed."
      }
    }'
    exit 0
fi

exit 0
