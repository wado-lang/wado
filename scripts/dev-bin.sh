#!/usr/bin/env bash
# Run a workspace binary (`wado`, `wado-dev-tools`) from the dev build, building
# it first. It runs in the caller's directory, as under `cargo run`; the build
# runs at the root, since cargo reads `.cargo/config.toml` from the directory it
# runs in, and `wado-bundled-icu/`'s targets wasm32.
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
# A relative CARGO_TARGET_DIR names a directory under the caller's, so it is
# made absolute before cargo runs at the root.
target_dir="${CARGO_TARGET_DIR:-${root}/target}"
case "${target_dir}" in
    /*) ;;
    *) target_dir="${PWD}/${target_dir}" ;;
esac
exe="${target_dir}/debug/${bin}"

if [ -z "${WADO_PREBUILT:-}" ]; then
    (cd "${root}" && CARGO_TARGET_DIR="${target_dir}" cargo build --bin "${bin}") >&2
elif [ ! -x "${exe}" ]; then
    echo "error: WADO_PREBUILT is set, but ${exe} was not built" >&2
    exit 1
fi

if [ -n "${print_path}" ]; then
    echo "${exe}"
else
    exec "${exe}" "$@"
fi
