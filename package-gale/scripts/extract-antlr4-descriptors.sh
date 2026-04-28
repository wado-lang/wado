#!/usr/bin/env bash
# Wrapper around package-gale/scripts/extract_antlr4_descriptors.wado.
#
# Resolves the category list (default = Phase 1 set, or "all" to walk
# every category) and forwards to the Wado script. The Wado script
# itself creates the output directories and emits the .g4/.wado files.
#
# Usage (from anywhere):
#   package-gale/scripts/extract-antlr4-descriptors.sh [Category ...]
#   package-gale/scripts/extract-antlr4-descriptors.sh all
#
# Default categories (Phase 1): Sets LexerExec ParseTrees.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

DEFAULT_CATEGORIES=(Sets LexerExec ParseTrees)
DESCRIPTORS_ROOT="vendor/antlr4/runtime-testsuite/resources/org/antlr/v4/test/runtime/descriptors"

cd "$REPO_ROOT"

if [ ! -d "$DESCRIPTORS_ROOT" ]; then
    echo "extract: cannot find $DESCRIPTORS_ROOT" >&2
    echo "extract: the antlr4 submodule appears to be missing." >&2
    echo "extract: run: git submodule update --init --recommend-shallow vendor/antlr4" >&2
    exit 1
fi

# Resolve categories. "all" expands to every directory under the descriptor root.
if [ $# -eq 0 ]; then
    categories=("${DEFAULT_CATEGORIES[@]}")
elif [ "$1" = "all" ]; then
    categories=()
    for d in "$DESCRIPTORS_ROOT"/*/; do
        categories+=("$(basename "$d")")
    done
else
    categories=("$@")
fi

# Forward to the Wado script. `wado run` opens the cwd as the only preopen,
# so all paths are interpreted relative to $REPO_ROOT.
exec cargo run --quiet --bin wado -- run \
    package-gale/scripts/extract_antlr4_descriptors.wado -- "${categories[@]}"
