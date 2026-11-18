#!/bin/bash
# Installed by stop-hook-override.sh in place of the launcher's Stop hook.

input=$(cat)
[[ "$input" =~ \"stop_hook_active\"[[:space:]]*:[[:space:]]*true ]] && exit 0
git rev-parse --git-dir >/dev/null 2>&1 || exit 0
[[ -n "$(git remote)" ]] || exit 0

changed=$(git status --porcelain)
if [[ -n "$changed" ]]; then
  {
    echo "The working tree has $(wc -l <<<"$changed") changed or untracked file(s):"
    head -20 <<<"$changed"
    echo "If a task that generates files is still running, commit nothing until it ends."
    echo "Otherwise commit the files that belong to the change, leaving out stray ones, and push."
  } >&2
  exit 2
fi

branch=$(git branch --show-current)
if [[ -n "$branch" ]] && git rev-parse -q --verify "origin/$branch" >/dev/null &&
  [[ -n "$(git rev-list -n 1 "origin/$branch..HEAD")" ]]; then
  echo "Branch '$branch' has commits not on origin. Push them." >&2
  exit 2
fi
exit 0
