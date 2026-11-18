#!/bin/bash
# The cloud launcher's Stop hook demands a commit at every stop, even while a
# job is still writing generated files, so a long run ends up as dozens of
# partial commits, each push cancelling CI. Its signature check misfires too.
# Replace it with stop-git-check.sh, which says to wait for such a job.
set -euo pipefail

[[ "${CLAUDE_CODE_REMOTE:-}" == "true" ]] || exit 0
target="$HOME/.claude/stop-hook-git-check.sh"
[[ -f "$target" ]] || exit 0
install -m 755 "${CLAUDE_PROJECT_DIR:-.}/.claude/hooks/stop-git-check.sh" "$target"
