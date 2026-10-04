#!/usr/bin/env bash
# PreToolUse hook for the `mcp__github__create_pull_request` tool.
#
# Enforces the repo's PR conventions at the deterministic point a PR is created,
# independent of whether the model invoked the pull-request skill. The
# conventions themselves are read from the pull-request skill so this hook never
# drifts from the documented source of truth:
#   - Title MUST follow Conventional Commits (the allowed types are derived from
#     the skill's title list).
#   - The skill body is surfaced as the description reminder.
#
# An invalid title is denied with the conventions; a valid title is allowed and
# the conventions are injected as a reminder. If the skill file is missing the
# hook fails open (allows) rather than blocking all PR creation.

set -euo pipefail

input=$(cat)
title=$(printf '%s' "$input" | jq -r '.tool_input.title // ""')

script_dir=$(cd "$(dirname "$0")" && pwd)
skill="$script_dir/../skills/pull-request/SKILL.md"

allow() {
    # $1: additionalContext (may be empty)
    jq -nc --arg ctx "$1" \
        '{hookSpecificOutput: ({hookEventName: "PreToolUse"} + (if $ctx == "" then {} else {additionalContext: $ctx} end))}'
    exit 0
}

# Fail open if the source of truth is unavailable.
[[ -f "$skill" ]] || allow ""

# Conventions surfaced to the model = the skill body (frontmatter stripped).
conventions=$(awk '/^---$/{c++; next} c>=2' "$skill")

# Allowed Conventional Commits types, derived from the skill's title list
# (e.g. "- \`feat\`: ..." / "- \`feat!\`: ..." -> feat).
types=$(sed -nE 's/^- `([a-z]+)!?`.*/\1/p' "$skill" | sort -u | paste -sd'|' -)

deny() {
    jq -nc --arg r "$1" \
        '{hookSpecificOutput: {hookEventName: "PreToolUse", permissionDecision: "deny", permissionDecisionReason: $r}}'
    exit 0
}

# An empty list means the skill's format changed under this parser; checking
# nothing would let every title through unnoticed.
[[ -n "$types" ]] || deny "pr-conventions.sh parsed no title types from $skill; fix the hook or the skill's list."

cc_regex="^(${types})(\([^)]+\))?!?: .+"

if [[ "$title" =~ $cc_regex ]]; then
    allow "$conventions"
fi

deny "PR title is not Conventional Commits: \"$title\".

$conventions"
