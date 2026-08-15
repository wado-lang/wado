#!/usr/bin/env bash
# PreToolUse hook for the Bash tool: bound target/debug/incremental.
#
# rustc prunes stale sessions inside one crate's directory, but nothing bounds
# the total and cargo has no target/ collector (`-Z gc` is nightly, and collects
# the registry rather than target/). Left alone the directory reaches several GB
# against a session's fixed disk allowance.
#
# Runs before the command rather than after it, so the build about to start is
# the one that gets the headroom. Eviction is per crate directory, least
# recently used first, down to a fraction of the cap: the crate being edited
# keeps its incremental state while crates the session has moved on from pay.

set -euo pipefail

LIMIT_GB=${WADO_INCREMENTAL_LIMIT_GB:-10}
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
INCREMENTAL="$TARGET_DIR/debug/incremental"
BUILD_LOCK="$TARGET_DIR/debug/.cargo-build-lock"

[ -d "$INCREMENTAL" ] || exit 0

limit=$((LIMIT_GB * 1024 * 1024 * 1024))
total=$(du -sB1 "$INCREMENTAL" | cut -f1)
[ "$total" -gt "$limit" ] || exit 0

# Read-write rather than truncating: cargo owns this file, we only lock it.
exec 9<>"$BUILD_LOCK"
# A build holds it exclusively, and rustc is writing these directories.
flock --exclusive --nonblock 9 || exit 0

goal=$((limit * 7 / 10))
evicted=0
while IFS=$'\t' read -r _ dir; do
    if [ "$total" -le "$goal" ]; then
        break
    fi
    size=$(du -sB1 "$dir" | cut -f1)
    rm -rf "$dir"
    total=$((total - size))
    evicted=$((evicted + 1))
done < <(find "$INCREMENTAL" -mindepth 1 -maxdepth 1 -type d -printf '%T@\t%p\n' | sort -n)

echo "[incremental-gc] evicted $evicted crate dirs over the ${LIMIT_GB} GB cap;" \
    "$((total / 1024 / 1024)) MB left" >&2
