#!/usr/bin/env bash
# PreToolUse hook for the Bash tool: block `sed`, `awk`, `python` and `python3`.
# `permissions.deny` covers the plain invocations; the pattern below also
# catches a pipeline stage, `xargs`, `find -exec`, `$(...)` and `/usr/bin/sed`.

set -euo pipefail

# Where a command word starts: the string, a shell operator, or a runner that
# takes a command as its argument.
delimiter='(^|[|&;({]|`|\$\(|[[:space:]](-exec|-execdir|xargs|sudo|env|command|time|nohup|watch)[[:space:]]+)'

exec "$(dirname "$0")/deny-bash-match.sh" \
    "${delimiter}[[:space:]]*([^[:space:]|&;]*/)?(sed|awk|python3?(\.[0-9]+)?)([[:space:]]|$)" \
    "sed, awk, python and python3 are forbidden in this repository (AGENTS.md > Tooling). Edit files with the editing tools, one call per change site; read with the Read tool and search with the Grep tool; script in Node.js (node, or an executable .mts as in .claude/hooks/). The ban is about the tool, not the care taken with it: exact-match asserts do not save it. A sed -i or a python3 heredoc that rewrites a file keeps matching where it was not aimed — a CLAUDE.md symlink replaced by a regular file, a migration table rewritten into nonsense, .rs hit when only .wado was meant — and each miss costs a diagnosis-and-revert cycle. For a rename too wide to do one call at a time, agree on the approach first. Naming one of them mid-command trips this too, even inside a quoted string — put such text in a file instead."
