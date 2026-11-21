#!/usr/bin/env bash
# Run a workspace binary (`wado`, `wado-dev-tools`) from the dev build, building
# it first. It runs in the caller's directory, as under `cargo run`; the build
# runs at the root, since cargo reads `.cargo/config.toml` from the directory it
# runs in, and `wado-bundled-icu/`'s targets wasm32.
#
#   scripts/dev-bin.sh wado test -O2 package-gale
#   WADO=$(scripts/dev-bin.sh --path wado)    # build once, then run "$WADO" in a loop
#   scripts/dev-bin.sh --workspace wado-run-webgpu wado-run-webgpu app.wado
#
# `--workspace` names a workspace of its own under the root, which builds into
# its own `target/` unless CARGO_TARGET_DIR says otherwise.
#
# With WADO_PREBUILT set, the binary is taken as built: a CI job that was handed
# it by `build-wado`, or a caller that built it once and fans out. A missing one
# fails rather than compiles, since compiling it again is the cost the handoff
# exists to remove.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
print_path=
while true; do
    case "$1" in
        --path) print_path=1 ;;
        --workspace)
            root="${root}/$2"
            shift
            ;;
        *) break ;;
    esac
    shift
done
bin=$1
shift

# A relative CARGO_TARGET_DIR names a directory under the caller's, so it is
# made absolute before cargo runs at the root. Without one, cargo's own answer
# covers a `build.target-dir` in its configuration.
if [ -n "${CARGO_TARGET_DIR:-}" ]; then
    case "${CARGO_TARGET_DIR}" in
        /*) target_dir="${CARGO_TARGET_DIR}" ;;
        *) target_dir="${PWD}/${CARGO_TARGET_DIR}" ;;
    esac
else
    target_dir="$(cd "${root}" && cargo metadata --format-version 1 --no-deps --offline |
        grep -o '"target_directory":"[^"]*"' | cut -d'"' -f4)"
fi
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
