#!/usr/bin/env bash
set -euo pipefail

# Copy `onnx.proto`, which Loam's reader imports through Grog, and the ONNX
# backend test data Loam's tests read out of the `vendor/onnx` submodule.
#
# The tests say which files: every `../tests/onnx/...` path they name is
# fetched, so naming a new model in a test and running this brings it in. A
# file here that no test names is reported, not deleted.
#
# Usage: mise run sync-onnx-data

SRC=vendor/onnx/onnx/backend/test/data
DEST=package-loam/tests/onnx

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
    if ! cmp -s "${SRC}/${rel}" "${DEST}/${rel}"; then
        cp "${SRC}/${rel}" "${DEST}/${rel}"
        echo "updated ${rel}"
        changed=$((changed + 1))
    fi
done <<<"$wanted"

echo "==> ${changed} changed of $(echo "$wanted" | wc -l | tr -d ' ') named by a test"

PROTO=package-loam/src/onnx.proto
if ! cmp -s vendor/onnx/onnx/onnx.proto "$PROTO"; then
    cp vendor/onnx/onnx/onnx.proto "$PROTO"
    echo "updated ${PROTO}"
fi

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
