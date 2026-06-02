#!/usr/bin/env bash
# Three-phase Gale antlr4 descriptor pipeline:
#   1. Wado extract (Stage A/B drivers + Stage B′ manifest)
#   2. Stage B′ oracle invocation (strip action bodies, run jar TestRig)
#   3. Wado finalize (drain oracle-pending into stage_b_oracle/)
#
# Phase 2 is skipped with a warning when java/javac is missing, so
# Stage A/B regeneration still works on a JDK-less environment.
#
# Usage:
#   extract-antlr4-descriptors.sh                # all categories
#   extract-antlr4-descriptors.sh Sets LexerExec # subset

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

DESCRIPTORS_ROOT="vendor/antlr4/runtime-testsuite/resources/org/antlr/v4/test/runtime/descriptors"
COMPAT_ROOT="package-gale/tests/antlr4-compat"
PENDING_ROOT="$COMPAT_ROOT/oracle-pending"
GRAMMARS_ROOT="$COMPAT_ROOT/grammars"

cd "$REPO_ROOT"

if [ ! -d "$DESCRIPTORS_ROOT" ]; then
    echo "extract: cannot find $DESCRIPTORS_ROOT" >&2
    echo "extract: the antlr4 submodule appears to be missing." >&2
    echo "extract: run: git submodule update --init --recommend-shallow vendor/antlr4" >&2
    exit 1
fi

# Resolve categories. No args (or "all") expands to every directory under
# the descriptor root.
if [ $# -eq 0 ] || [ "$1" = "all" ]; then
    categories=()
    for d in "$DESCRIPTORS_ROOT"/*/; do
        categories+=("$(basename "$d")")
    done
else
    categories=("$@")
fi

# ----- Phase 1: Wado extract.
echo "==> Phase 1/3: Wado extract"
cargo run --quiet --bin wado -- run \
    package-gale/scripts/extract_antlr4_descriptors.wado -- "${categories[@]}"

# ----- Phase 2: Stage B′ oracle invocation.
echo "==> Phase 2/3: Stage B′ oracle invocation"
if ! command -v java >/dev/null 2>&1 || ! command -v javac >/dev/null 2>&1; then
    echo "extract: skipping Stage B′ oracle invocation — neither 'java' nor 'javac' on PATH." >&2
    echo "extract: install a JDK 17+ (e.g. via 'mise install java') to enable Stage B′." >&2
else
    if [ ! -d "$PENDING_ROOT" ]; then
        echo "extract: no oracle-pending/ — nothing to invoke the oracle on."
    else
        tmpdir=$(mktemp -d -t gale-oracle-loop.XXXXXX)
        trap 'rm -rf "$tmpdir"' EXIT

        pending=0
        ok=0
        oracle_err=0
        strip_err=0
        missing_g4=0

        while IFS= read -r -d '' input_path; do
            pending=$((pending + 1))
            rel="${input_path#$PENDING_ROOT/}"
            rel_base="${rel%.input}"
            category="${rel_base%/*}"
            name="${rel_base##*/}"
            start_path="$PENDING_ROOT/$rel_base.start"
            expected_path="$PENDING_ROOT/$rel_base.expected"
            g4_path="$GRAMMARS_ROOT/$category/$name.g4"

            if [ ! -f "$start_path" ]; then
                echo "  $category/$name: missing .start — skipping" >&2
                rm -f "$expected_path"
                continue
            fi
            if [ ! -f "$g4_path" ]; then
                echo "  $category/$name: grammar file missing at $g4_path" >&2
                rm -f "$expected_path"
                missing_g4=$((missing_g4 + 1))
                continue
            fi

            start_rule=$(cat "$start_path")

            stripped_g4="$tmpdir/$name.g4"
            if ! cargo run --quiet --bin wado -- run \
                    --dir "$GRAMMARS_ROOT/$category" \
                    -- "$SCRIPT_DIR/strip-grammar.wado" "$name.g4" \
                    > "$stripped_g4" 2> "$tmpdir/strip.err"; then
                echo "  $category/$name: strip-grammar failed" >&2
                sed 's/^/    /' "$tmpdir/strip.err" >&2
                rm -f "$expected_path"
                strip_err=$((strip_err + 1))
                continue
            fi

            # exit 0 = clean tree; 1 = invocation error; 2 = tree with
            # ANTLR4-reported error markers (not a useful pin).
            set +e
            ANTLR4_VERSION="${ANTLR4_VERSION:-}" \
                "$SCRIPT_DIR/antlr4-oracle.sh" "$stripped_g4" "$start_rule" \
                    < "$input_path" \
                    > "$expected_path" \
                    2> "$tmpdir/oracle.err"
            oracle_rc=$?
            set -e

            if [ $oracle_rc -ne 0 ]; then
                rm -f "$expected_path"
                head_msg=$(head -n5 "$tmpdir/oracle.err" 2>/dev/null | sed 's/^/    /')
                echo "  $category/$name: oracle exit=$oracle_rc" >&2
                if [ -n "$head_msg" ]; then
                    printf '%s\n' "$head_msg" >&2
                fi
                oracle_err=$((oracle_err + 1))
                continue
            fi

            ok=$((ok + 1))
        done < <(find "$PENDING_ROOT" -name '*.input' -print0 2>/dev/null || true)

        echo "extract: oracle invoked on $pending pending entries:"
        echo "extract:           $ok ok"
        echo "extract:           $oracle_err oracle failures (descriptor needs [stage_b_oracle_skip])"
        echo "extract:           $strip_err strip-grammar failures"
        echo "extract:           $missing_g4 missing .g4 files"
    fi
fi

# ----- Phase 3: Wado finalize.
echo "==> Phase 3/3: Wado finalize stage_b_oracle/"
# Stamp emitted tests with the jar version phase 2 actually used.
oracle_version_cache="${XDG_CACHE_HOME:-$HOME/.cache}/gale/antlr4-latest-version"
if [ -f "$oracle_version_cache" ]; then
    ORACLE_VERSION=$(cat "$oracle_version_cache")
    export ORACLE_VERSION
fi
cargo run --quiet --bin wado -- run \
    package-gale/scripts/extract_antlr4_descriptors.wado -- \
    --finalize-stage-b-oracle
