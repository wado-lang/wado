#!/usr/bin/env bash
# Build the named binaries (`wado`, `wado-dev-tools`) into target/debug, where
# the caller runs them from.
#
# CI builds them once, in its `build-wado` job, and hands them to every job that
# runs them. Those jobs set WADO_PREBUILT, and this script then only checks they
# arrived: a second compile per job is what the handoff exists to remove.
#
#   scripts/build-dev-bins.sh wado
#   scripts/build-dev-bins.sh wado wado-dev-tools
set -euo pipefail

cd "$(dirname "$0")/.."

if [ -n "${WADO_PREBUILT:-}" ]; then
    for bin in "$@"; do
        if [ ! -x "target/debug/${bin}" ]; then
            echo "error: WADO_PREBUILT is set, but target/debug/${bin} was not handed over" >&2
            exit 1
        fi
    done
    exit 0
fi

args=()
for bin in "$@"; do
    args+=(--bin "${bin}")
done
exec cargo build "${args[@]}"
