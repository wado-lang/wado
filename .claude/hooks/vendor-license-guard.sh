#!/usr/bin/env bash
# PreToolUse hook for the Bash tool: keep implementation source under vendor/
# unread, where a package's AGENTS.md forbids it. `permissions.deny` covers the
# Read tool; this covers `cat`, `grep`, `find -exec` and friends, which reach the
# same bytes. Naming such a path in a command trips it, even when not reading it.

set -euo pipefail

input=$(cat)
cmd=$(printf '%s' "$input" | jq -r '.tool_input.command // empty')

if [ -z "$cmd" ]; then
    exit 0
fi

deny() {
    jq -n --arg reason "$1" '{
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        permissionDecision: "deny",
        permissionDecisionReason: $reason
      }
    }'
    exit 0
}

# Anchored on `vendor/antlr4/` so a `runtime/` elsewhere in the repo is untouched.
if printf '%s' "$cmd" | grep -Eq 'vendor/antlr4/(tool/[^[:space:]]*\.(java|g)|runtime/[^[:space:]]*\.java)'; then
    deny "License hygiene (package-gale/AGENTS.md): ANTLR4 implementation source under vendor/antlr4/{tool,runtime} must not be read. To settle what ANTLR does, run the published jar as a black box (\`mise run oracle-gale\`, package-gale/scripts/antlr4_oracle.wado) or read vendor/antlr4/doc/*.md. Reading .g4 files and runtime-testsuite descriptors is allowed. Use the Edit tool to edit a file naming such a path."
fi

# ONNX: headers, the .proto files, the docs and the tests may be read; the
# implementation may not. Each path named is judged on its own, since one
# command may name a test and a source file both.
onnx_tests='^vendor/onnx/(tests/|onnx/backend/test/|onnx/fuzz/)'
onnx_source='^vendor/onnx/(tools/|docs/docsgen/[^[:space:]]*\.py|onnx/(reference|backend/sample|bin|tools)(/|$)|onnx/([^[:space:]]*/)?[^/[:space:]]*\.(py|cc|cpp|c)([^[:alnum:]]|$))'
while IFS= read -r path; do
    if printf '%s' "$path" | grep -Eq "$onnx_tests"; then
        continue
    fi
    if printf '%s' "$path" | grep -Eq "$onnx_source"; then
        deny "License hygiene (package-loam/AGENTS.md): ONNX's implementation under vendor/onnx must not be read. Read the operator definitions in vendor/onnx/docs/*.md, the .proto files, the headers, and the tests under vendor/onnx/tests and vendor/onnx/onnx/backend/test. To settle a behavior, run onnxruntime as an oracle. Use the Edit tool to edit a file naming such a path."
    fi
done < <(printf '%s' "$cmd" | grep -Eo "vendor/onnx/[^[:space:]'\"]*" || true)

exit 0
