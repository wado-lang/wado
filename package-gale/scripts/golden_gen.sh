#!/usr/bin/env bash
# Golden generated-parser diff harness.
#
# Runs `gale gen` over every grammar in tests/grammars/ (pairing
# `*Lexer.g4` + `*Parser.g4` halves) and captures stdout+stderr per
# grammar into an output directory. Run it once to record a baseline,
# refactor parser_gen.wado / lexer_gen.wado, run it again into a second
# directory, and `diff -r` the two. Identical output proves the refactor
# preserved generated parsers byte-for-byte.
#
#   package-gale/scripts/golden_gen.sh /tmp/gold-before
#   ... edit ...
#   package-gale/scripts/golden_gen.sh /tmp/gold-after
#   diff -r /tmp/gold-before /tmp/gold-after
set -u
OUT="${1:?usage: golden_gen.sh <output-dir>}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
mkdir -p "$OUT"

cd "$ROOT/.."
shopt -s nullglob
gdir="package-gale/tests/grammars"

# Build once, then invoke the binary directly so each grammar skips
# cargo's per-invocation workspace freshness check.
cargo build --quiet --bin wado || exit 1
WADO="$ROOT/../target/debug/wado"

for g in "$gdir"/*.g4; do
    base="$(basename "$g" .g4)"
    case "$base" in
        *Parser)
            lex="$gdir/${base%Parser}Lexer.g4"
            if [ -f "$lex" ]; then
                args="$lex $g"; name="${base%Parser}"
            else
                args="$g"; name="$base"
            fi
            ;;
        *Lexer)
            par="$gdir/${base%Lexer}Parser.g4"
            # Parser half drives the Lexer pairing; skip the lone Lexer
            # when a Parser sibling exists (handled in the *Parser case).
            if [ -f "$par" ]; then continue; fi
            args="$g"; name="$base"
            ;;
        *)
            args="$g"; name="$base"
            ;;
    esac
    $WADO run package-gale gen $args > "$OUT/$name.out" 2> "$OUT/$name.err"
    echo "gen $name (exit $?)"
done
