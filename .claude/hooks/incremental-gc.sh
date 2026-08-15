#!/usr/bin/env bash
# PreToolUse hook for the Bash tool: spend target/debug/incremental to keep the
# session's disk allowance from running out. Nothing else bounds the directory
# -- rustc prunes only within one crate's, and cargo's `-Z gc` is nightly and
# collects the registry, not target/.
#
# Free space is the trigger rather than the directory's own size: a size cap
# discards state while the disk is still half empty, and says nothing about
# what else filled it. Running before the command hands the headroom to the
# build about to start, and evicting least-recently-used crate directories
# spares the one being edited.

set -euo pipefail

FLOOR_GB=${WADO_DISK_FLOOR_GB:-10}
MARGIN_GB=${WADO_DISK_MARGIN_GB:-5}
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
INCREMENTAL="$TARGET_DIR/debug/incremental"
BUILD_LOCK="$TARGET_DIR/debug/.cargo-build-lock"

[ -d "$INCREMENTAL" ] || exit 0

avail() {
    df --output=avail -B1 "$TARGET_DIR" | tail -1
}

gib() {
    awk -v v="$1" 'BEGIN { printf "%.0f", v * 1073741824 }'
}

floor=$(gib "$FLOOR_GB")
goal=$(gib "$(awk -v f="$FLOOR_GB" -v m="$MARGIN_GB" 'BEGIN { print f + m }')")
[ "$(avail)" -lt "$floor" ] || exit 0

# `<>` rather than `>`: cargo owns this file, we only lock it. A build holds it
# exclusively, and rustc is writing the directories below.
exec 9<>"$BUILD_LOCK"
flock --exclusive --nonblock 9 || exit 0

evicted=0
while IFS=$'\t' read -r _ dir; do
    if [ "$(avail)" -ge "$goal" ]; then
        break
    fi
    rm -rf "$dir"
    evicted=$((evicted + 1))
done < <(find "$INCREMENTAL" -mindepth 1 -maxdepth 1 -type d -printf '%T@\t%p\n' | sort -n)

if [ "$evicted" -gt 0 ]; then
    echo "[incremental-gc] evicted $evicted crate dirs below the ${FLOOR_GB} GB floor;" \
        "$(($(avail) / 1024 / 1024 / 1024)) GB free" >&2
fi
exit 0
