#!/usr/bin/env bash
# Run a workspace binary (`wado`, `wado-dev-tools`) from the dev build, building
# it first. It runs in the caller's directory, as under `cargo run`.
#
#   scripts/dev-bin.sh wado test -O2 package-gale
#   WADO=$(scripts/dev-bin.sh --path wado)    # build once, then run "$WADO" in a loop
#
# With WADO_PREBUILT set, the binary is taken as built: a CI job that was handed
# it by `build-wado`, or a caller that built it once and fans out. A missing one
# fails rather than compiles, since compiling it again is the cost the handoff
# exists to remove.
set -euo pipefail

print_path=
if [ "$1" = --path ]; then
    print_path=1
    shift
fi
bin=$1
shift

root="$(cd "$(dirname "$0")/.." && pwd)"
# A relative CARGO_TARGET_DIR is relative to the directory cargo runs in, which
# is the caller's here too.
exe="${CARGO_TARGET_DIR:-${root}/target}/debug/${bin}"

if [ -z "${WADO_PREBUILT:-}" ]; then
    cargo build --manifest-path "${root}/Cargo.toml" --bin "${bin}" >&2
elif [ ! -x "${exe}" ]; then
    echo "error: WADO_PREBUILT is set, but ${exe} was not built" >&2
    exit 1
fi

if [ -n "${print_path}" ]; then
    echo "${exe}"
else
    exec "${exe}" "$@"
fi
