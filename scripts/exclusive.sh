#!/usr/bin/env bash
# Run `"$@"` only while no other invocation under `$1` holds the lock.
#
# Two core-saturating runs do not fail, they starve each other: the newcomer
# spends its time rebuilding against a machine the stale one owns, and the
# silence reads as a hang rather than as contention. Refusing the second is what
# makes that visible.
set -euo pipefail

name=$1
shift

# Degrade loudly rather than refuse: the lock catches a mistake, it is not a
# correctness requirement, and macOS ships no `flock` — failing closed there
# would leave the suite unrunnable to enforce a convenience.
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
: >"$lock"
echo $$ >&9

exec "$@"
