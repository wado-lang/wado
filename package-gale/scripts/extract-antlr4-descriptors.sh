#!/usr/bin/env bash
# Wrapper around package-gale/scripts/extract_antlr4_descriptors.wado.
#
# Pre-creates the per-category output directories (the Wado script avoids
# `create_directory_at` until the underlying compiler bug is fixed — see
# `wado-compiler/tests/fixtures/result_unit_cm_variant_passthrough.wado`).
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

# `wado compile` lowers some `Result<unit, CmVariant>` shapes incorrectly
# when the entry filename is given as a relative path (see
# `wado-compiler/tests/fixtures/result_unit_cm_variant_passthrough.wado`
# for the underlying compiler bug). The Wado script avoids those shapes
# directly, but the entry file itself must still be passed as an
# absolute path or `cargo run --bin wado -- run` will hit the same
# path-dependent monomorph-registration order on the script's WIR.
SCRIPT_ABS="$REPO_ROOT/package-gale/scripts/extract_antlr4_descriptors.wado"

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

# mkdir -p the output dirs the Wado script will write into.
mkdir -p package-gale/tests/antlr4_descriptors
for cat in "${categories[@]}"; do
    mkdir -p "package-gale/tests/antlr4_descriptors/$cat"
done

# Forward to the Wado script. `wado run` opens the cwd as the only preopen,
# so all paths are interpreted relative to $REPO_ROOT.
exec cargo run --quiet --bin wado -- run \
    "$SCRIPT_ABS" -- "${categories[@]}"
