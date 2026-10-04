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
# the conventions are injected as a reminder. A missing skill file, or a
# `## Title` section with no types, is denied: the source of truth moved or
# changed under this parser, and checking nothing would pass every title.

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

deny() {
    jq -nc --arg r "$1" \
        '{hookSpecificOutput: {hookEventName: "PreToolUse", permissionDecision: "deny", permissionDecisionReason: $r}}'
    exit 0
}

[[ -f "$skill" ]] || deny "pr-conventions.sh cannot find $skill; fix the hook's path."

# Conventions surfaced to the model = the skill body (frontmatter stripped).
conventions=$(awk '/^---$/{c++; next} c>=2' "$skill")

# Allowed Conventional Commits types: the "- \`feat\`" bullets of the skill's
# `## Title` section only, so a bullet elsewhere never becomes a type.
types=$(awk '/^## /{in_title=($0 == "## Title"); next} in_title' "$skill" \
    | sed -nE 's/^- `([a-z]+)!?`.*/\1/p' | sort -u | paste -sd'|' -)

[[ -n "$types" ]] || deny "pr-conventions.sh parsed no title types from $skill; fix the hook or the skill's list."

cc_regex="^(${types})(\([^)]+\))?!?: .+"

if [[ "$title" =~ $cc_regex ]]; then
    allow "$conventions"
fi

deny "PR title is not Conventional Commits: \"$title\".

$conventions"
