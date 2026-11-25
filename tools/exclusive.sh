#!/usr/bin/env bash
# Run `"$@"` unless another invocation under `$1` holds the lock. Two
# core-saturating runs do not fail, they starve each other, and the silence of
# the second reads as a hang.
set -euo pipefail

name=$1
shift

# The lock catches a mistake rather than guarding correctness, so a host without
# `flock` runs unlocked and says so.
if ! command -v flock >/dev/null; then
    echo "mise: no flock, running '$name' unlocked — a second one will not be refused." >&2
    exec "$@"
fi

mkdir -p target
lock="target/.$name.lock"
exec 9>>"$lock"
if ! flock -n 9; then
    # Report who holds the lock now: an orphaned test binary inherits fd 9 and
    # outlives the pid this file records. Close our own handle first, or `fuser`
    # counts this script and its pipeline among the holders. Nothing below needs
    # the descriptor — the lock is not ours and this path only reports and exits.
    exec 9>&-
    holders=""
    if command -v fuser >/dev/null; then
        holders=$(fuser "$lock" 2>/dev/null | tr -s '[:space:]' ' ' || true)
        holders=${holders# }
        holders=${holders% }
    fi
    if [ -n "$holders" ]; then
        echo "mise: '$name' is already running (pid $holders)." >&2
        echo "mise: kill it first — kill $holders" >&2
    else
        recorded=$(cat "$lock" 2>/dev/null || true)
        echo "mise: '$name' is already running${recorded:+, started by pid $recorded}." >&2
        echo "mise: find its holder — fuser -v $lock" >&2
    fi
    exit 1
fi
# Truncate through a second open: reopening fd 9 would drop the lock with it.
: >"$lock"
echo $$ >&9

exec "$@"
