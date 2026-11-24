#!/usr/bin/env bash
set -euo pipefail

# Copy out of the `vendor/onnx` submodule the `onnx.proto` Loam's reader imports
# through Grog, and the backend test data Loam's tests read.
#
# The tests say which files: every `../tests/onnx/...` path they name is
# fetched, so naming a new model in a test and running this brings it in. A
# file here that no test names is reported, not deleted.
#
# Usage: mise run sync-onnx-data

ONNX=vendor/onnx/onnx
SRC=${ONNX}/backend/test/data
DEST=package-loam/tests/onnx

# Copy $1 over $2 where they differ, saying so; return 1 where they already agree.
sync_file() {
    if cmp -s "$1" "$2"; then
        return 1
    fi
    cp "$1" "$2"
    echo "updated $2"
}

if [ ! -d "$SRC" ]; then
    echo "ERROR: ${SRC} is missing." >&2
    echo "Run: git submodule update --init --depth 1 vendor/onnx" >&2
    exit 1
fi

wanted=$(
    grep -rhoE '\.\./tests/onnx/[^"]+' --include='*.wado' --exclude-dir=build package-loam |
        while IFS= read -r named; do echo "${named#../tests/onnx/}"; done | sort -u
)

if [ -z "$wanted" ]; then
    echo "ERROR: no test under package-loam names a file in ${DEST}" >&2
    exit 1
fi

changed=0
while IFS= read -r rel; do
    # A test names a file under `tests/onnx`, so a path climbing out of it would
    # have this script read and write somewhere neither directory covers.
    case "/${rel}/" in
    */../* | //*)
        echo "ERROR: ${rel} leaves ${DEST}" >&2
        exit 1
        ;;
    esac
    if [ ! -f "${SRC}/${rel}" ]; then
        echo "ERROR: ${SRC}/${rel} is missing; the submodule may be at another commit" >&2
        exit 1
    fi
    mkdir -p "${DEST}/$(dirname "$rel")"
    if sync_file "${SRC}/${rel}" "${DEST}/${rel}"; then
        changed=$((changed + 1))
    fi
done <<<"$wanted"

echo "==> ${changed} changed of $(echo "$wanted" | wc -l | tr -d ' ') named by a test"

sync_file "${ONNX}/onnx.proto" package-loam/src/onnx.proto || true

while IFS= read -r have; do
    rel=${have#"${DEST}/"}
    if ! echo "$wanted" | grep -qxF "$rel"; then
        echo "stale: ${rel} (no test names it)"
    fi
done < <(find "$DEST" -type f ! -name README.md | sort)

# The README records where these came from, and nothing else checks it.
head=$(git -C vendor/onnx rev-parse HEAD)
if ! grep -qF "$head" "${DEST}/README.md"; then
    echo "==> the submodule is at ${head}"
    echo "    ${DEST}/README.md records another commit; update it"
fi
