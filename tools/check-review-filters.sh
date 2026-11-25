#!/usr/bin/env bash
set -euo pipefail

# Hold `.coderabbit.yaml`'s `path_filters` to `.gitattributes`. A path marked
# `linguist-generated` or `linguist-vendored` is regenerated or fetched, so a
# review skips it, and the two files have to agree on which those are.
#
# A gitattributes pattern with no `/` matches at any depth, which the reviewer's
# globs spell `**/`.
#
# Usage: mise run check-review-filters

cd "$(dirname "$0")/.."

missing=0
while IFS= read -r pattern; do
    case "$pattern" in
    */*) glob="!${pattern}" ;;
    *) glob="!**/${pattern}" ;;
    esac
    if ! grep -qxF "    - \"${glob}\"" .coderabbit.yaml; then
        echo "missing from .coderabbit.yaml: - \"${glob}\"" >&2
        missing=$((missing + 1))
    fi
done < <(grep -vE '^[[:space:]]*#' .gitattributes |
    grep -E 'linguist-(generated|vendored)' | cut -d' ' -f1)

if [ "$missing" -gt 0 ]; then
    echo "" >&2
    echo "${missing} path(s) marked in .gitattributes are not filtered out of review." >&2
    exit 1
fi

echo "path_filters cover every generated and vendored path"
