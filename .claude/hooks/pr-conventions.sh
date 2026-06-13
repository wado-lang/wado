#!/usr/bin/env bash
# PreToolUse hook for the `mcp__github__create_pull_request` tool.
#
# Enforces the repo's PR conventions (see .claude/skills/pull-request) at the
# deterministic point a PR is created, independent of whether the model invoked
# the pull-request skill:
#   - Title MUST follow Conventional Commits.
#   - Description conventions are surfaced (outcome of origin/main...HEAD; no
#     trial-and-error history; no test section).
#
# An invalid title is denied with the conventions; a valid title is allowed and
# the description conventions are injected as a reminder.

set -euo pipefail

input=$(cat)
title=$(printf '%s' "$input" | jq -r '.tool_input.title // ""')

# Conventional Commits: type(scope)?!?: subject
cc_regex='^(feat|fix|docs|perf|refactor|chore)(\([^)]+\))?!?: .+'

conventions='PR conventions (.claude/skills/pull-request):
- Title: Conventional Commits — feat|fix|docs|perf|refactor|chore, optional (scope), optional ! for a breaking change. e.g. "feat(optimizer): add pass", "refactor!: drop X".
- Description: the outcome of the whole branch (git diff origin/main...HEAD, three dots). No trial-and-error history — the commits carry that. No test section — CI runs the suite.'

if [[ "$title" =~ $cc_regex ]]; then
    jq -nc --arg ctx "$conventions" '{
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        additionalContext: $ctx
      }
    }'
    exit 0
fi

reason="PR title is not Conventional Commits: \"$title\".

$conventions"
jq -nc --arg r "$reason" '{
  hookSpecificOutput: {
    hookEventName: "PreToolUse",
    permissionDecision: "deny",
    permissionDecisionReason: $r
  }
}'
