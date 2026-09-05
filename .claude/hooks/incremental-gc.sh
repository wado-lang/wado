#!/usr/bin/env bash
# PreToolUse hook for the Bash tool: nothing else bounds target/, so once free
# space falls below the floor, spend rebuildable output least-recently-used
# first -- incremental state, then the stale test binaries in deps/.

set -euo pipefail

FLOOR_GB=${WADO_DISK_FLOOR_GB:-12}
MARGIN_GB=${WADO_DISK_MARGIN_GB:-5}
# Below this age a binary is likely the current build's own output.
DEPS_MIN_AGE_MIN=${WADO_DEPS_MIN_AGE_MIN:-60}
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
INCREMENTAL="$TARGET_DIR/debug/incremental"
DEPS="$TARGET_DIR/debug/deps"
FINGERPRINTS="$TARGET_DIR/debug/.fingerprint"
BUILD_LOCK="$TARGET_DIR/debug/.cargo-build-lock"

[ -d "$TARGET_DIR/debug" ] || exit 0

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

# Debris first, costing nothing to lose: a split-DWARF object holds only the
# debug info its binary points at, and a killed linker leaves its scratch behind.
# Neither is an input to a build, and together they outweigh the rest here.
evicted_debris=0
if [ -d "$DEPS" ]; then
    while IFS=$'\t' read -r _ file; do
        [ "$(avail)" -lt "$goal" ] || break
        rm -f "$file"
        evicted_debris=$((evicted_debris + 1))
    done < <(find "$DEPS" -maxdepth 1 -type f \( -name '*.dwo' -o -name '*.tmp*' \) \
        -printf '%T@\t%p\n' | sort -n)
fi

evicted_dirs=0
if [ -d "$INCREMENTAL" ]; then
    while IFS=$'\t' read -r _ dir; do
        [ "$(avail)" -lt "$goal" ] || break
        rm -rf "$dir"
        evicted_dirs=$((evicted_dirs + 1))
    done < <(find "$INCREMENTAL" -mindepth 1 -maxdepth 1 -type d -printf '%T@\t%p\n' | sort -n)
fi

# Extensionless executables only: leaving the .rlib / .rmeta keeps the eviction
# to a relink. The fingerprint goes with the binary -- cargo reads freshness
# there -- paired by the trailing hash, the crate name being spelled differently.
#
# The build lock does not reach this. Cargo drops it once everything is
# compiled, and `cargo test` then spawns the binaries one at a time. Evicting
# one it has already judged fresh makes that spawn fail outright rather than
# rebuild. So any live cargo holds these back: none of them says which phase
# it is in.
evicted_bins=0
if [ "$(avail)" -lt "$goal" ] && [ -d "$DEPS" ] && ! pgrep -x cargo >/dev/null; then
    while IFS=$'\t' read -r _ bin; do
        [ "$(avail)" -lt "$goal" ] || break
        hash=${bin##*-}
        rm -f "$bin"
        if [ -n "$hash" ] && [ -d "$FINGERPRINTS" ]; then
            find "$FINGERPRINTS" -mindepth 1 -maxdepth 1 -type d -name "*-$hash" \
                -exec rm -rf {} + 2>/dev/null || true
        fi
        evicted_bins=$((evicted_bins + 1))
    done < <(find "$DEPS" -maxdepth 1 -type f -executable ! -name '*.*' \
        -mmin "+$DEPS_MIN_AGE_MIN" -printf '%T@\t%p\n' | sort -n)
fi

if [ "$evicted_debris" -gt 0 ] || [ "$evicted_dirs" -gt 0 ] || [ "$evicted_bins" -gt 0 ]; then
    echo "[incremental-gc] evicted $evicted_debris debug/scratch files," \
        "$evicted_dirs crate dirs and $evicted_bins stale test binaries" \
        "below the ${FLOOR_GB} GB floor; $(($(avail) / 1024 / 1024 / 1024)) GB free" >&2
fi
exit 0
