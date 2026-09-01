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
    holder=$(cat "$lock" 2>/dev/null || true)
    echo "mise: '$name' is already running${holder:+ (pid $holder)}." >&2
    echo "mise: kill it first — ps -eo pid,lstart,etime,pcpu,args | grep -w $name" >&2
    exit 1
fi
# Truncate through a second open: reopening fd 9 would drop the lock with it.
: >"$lock"
echo $$ >&9

exec "$@"
