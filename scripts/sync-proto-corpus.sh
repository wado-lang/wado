#!/usr/bin/env bash
set -euo pipefail

# Copy the `.proto` files Grog's tests read out of the `vendor/protobuf` and
# `vendor/onnx` submodules, which CI does not check out.
#
# The tests say which files: every `../tests/proto/<submodule>/...` path they
# name is fetched from `vendor/<submodule>/...`, so naming a new schema in a
# test and running this brings it in. A file here that no test names is
# reported, not deleted.
#
# Usage: mise run sync-proto-corpus

DEST=package-grog/tests/proto

wanted=$(
    grep -rhoE '\.\./tests/proto/[^"]+' --include='*.wado' --exclude-dir=build package-grog |
        while IFS= read -r named; do echo "${named#../tests/proto/}"; done | sort -u
)

if [ -z "$wanted" ]; then
    echo "ERROR: no test under package-grog names a file in ${DEST}" >&2
    exit 1
fi

changed=0
while IFS= read -r rel; do
    case "/${rel}/" in
    */../* | //*)
        echo "ERROR: ${rel} leaves ${DEST}" >&2
        exit 1
        ;;
    esac
    submodule=${rel%%/*}
    if [ ! -d "vendor/${submodule}/.git" ] && [ ! -f "vendor/${submodule}/.git" ]; then
        echo "ERROR: vendor/${submodule} is missing." >&2
        echo "Run: git submodule update --init --depth 1 vendor/${submodule}" >&2
        exit 1
    fi
    if [ ! -f "vendor/${rel}" ]; then
        echo "ERROR: vendor/${rel} is missing; the submodule may be at another commit" >&2
        exit 1
    fi
    mkdir -p "${DEST}/$(dirname "$rel")"
    if ! cmp -s "vendor/${rel}" "${DEST}/${rel}"; then
        cp "vendor/${rel}" "${DEST}/${rel}"
        echo "updated ${rel}"
        changed=$((changed + 1))
    fi
done <<<"$wanted"

echo "==> ${changed} changed of $(echo "$wanted" | wc -l | tr -d ' ') named by a test"

while IFS= read -r have; do
    rel=${have#"${DEST}/"}
    if ! echo "$wanted" | grep -qxF "$rel"; then
        echo "stale: ${rel} (no test names it)"
    fi
done < <(find "$DEST" -type f ! -name README.md | sort)

# The README records where these came from, and nothing else checks it.
for submodule in $(echo "$wanted" | cut -d/ -f1 | sort -u); do
    head=$(git -C "vendor/${submodule}" rev-parse HEAD)
    if ! grep -qF "$head" "${DEST}/README.md"; then
        echo "==> vendor/${submodule} is at ${head}"
        echo "    ${DEST}/README.md records another commit; update it"
    fi
done
