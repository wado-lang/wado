#!/usr/bin/env bash
# Run the published ANTLR4 jar as a clean-room oracle on a .g4 grammar +
# input. Used to validate descriptor-level expectations for ATN-class
# cases that Gale's static prediction cannot resolve. See the License
# hygiene section in package-gale/AGENTS.md: we may run the published
# jar to observe its black-box behavior, but we may not read ANTLR4's
# implementation source.
#
# Caches the jar at ~/.cache/gale/antlr-4.13.2-complete.jar on first
# use (downloads from antlr.org). Generates Java sources, compiles
# them, runs TestRig with -tree, and emits the resulting S-expression
# parse tree to stdout. The Java sources, .class files, and lexed
# tokens stay in a per-invocation temp directory that is removed on
# exit unless ORACLE_KEEP=1 is set.
#
# Usage:
#   package-gale/scripts/antlr4-oracle.sh <grammar.g4> <start_rule> < input
#   package-gale/scripts/antlr4-oracle.sh --tokens <grammar.g4> < input
#
# Options:
#   --tokens    print the token stream instead of the parse tree (lexer
#               oracle; <start_rule> is omitted).
#
# Exit codes:
#   0 = oracle ran and printed output
#   1 = invocation error (missing args, missing jar after download, etc.)
#   2 = ANTLR4 reported a parse error on the input

set -euo pipefail

ANTLR4_VERSION="4.13.2"
ANTLR4_URL="https://www.antlr.org/download/antlr-${ANTLR4_VERSION}-complete.jar"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/gale"
JAR_PATH="$CACHE_DIR/antlr-${ANTLR4_VERSION}-complete.jar"

usage() {
    cat >&2 <<EOF
Usage: $(basename "$0") <grammar.g4> <start_rule> < input
       $(basename "$0") --tokens <grammar.g4> < input
EOF
    exit 1
}

MODE="tree"
if [ "${1:-}" = "--tokens" ]; then
    MODE="tokens"
    shift
fi

if [ "$MODE" = "tree" ]; then
    [ $# -eq 2 ] || usage
    GRAMMAR_PATH="$1"
    START_RULE="$2"
else
    [ $# -eq 1 ] || usage
    GRAMMAR_PATH="$1"
    START_RULE="tokens"  # TestRig convention for lexer grammars
fi

if [ ! -f "$GRAMMAR_PATH" ]; then
    echo "oracle: cannot find grammar file: $GRAMMAR_PATH" >&2
    exit 1
fi

GRAMMAR_NAME="$(basename "$GRAMMAR_PATH" .g4)"

mkdir -p "$CACHE_DIR"
if [ ! -f "$JAR_PATH" ]; then
    echo "oracle: downloading $ANTLR4_URL → $JAR_PATH" >&2
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL -o "$JAR_PATH.tmp" "$ANTLR4_URL"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$JAR_PATH.tmp" "$ANTLR4_URL"
    else
        echo "oracle: neither curl nor wget is available" >&2
        exit 1
    fi
    mv "$JAR_PATH.tmp" "$JAR_PATH"
fi

if ! command -v java >/dev/null 2>&1; then
    echo "oracle: 'java' not on PATH" >&2
    exit 1
fi
if ! command -v javac >/dev/null 2>&1; then
    echo "oracle: 'javac' not on PATH (need JDK, not just JRE)" >&2
    exit 1
fi

WORK_DIR="$(mktemp -d -t gale-antlr4-oracle.XXXXXX)"
if [ "${ORACLE_KEEP:-0}" != "1" ]; then
    trap 'rm -rf "$WORK_DIR"' EXIT
else
    echo "oracle: keeping work dir: $WORK_DIR" >&2
fi

cp "$GRAMMAR_PATH" "$WORK_DIR/$GRAMMAR_NAME.g4"

# Generate Java sources. -no-listener avoids emitting Listener
# scaffolding we don't need.
if ! java -jar "$JAR_PATH" -Dlanguage=Java -no-listener -o "$WORK_DIR" "$WORK_DIR/$GRAMMAR_NAME.g4" >"$WORK_DIR/antlr.log" 2>&1; then
    echo "oracle: antlr4 codegen failed; see $WORK_DIR/antlr.log" >&2
    cat "$WORK_DIR/antlr.log" >&2
    exit 1
fi

# Compile the generated sources.
if ! (cd "$WORK_DIR" && javac -cp "$JAR_PATH" *.java) >"$WORK_DIR/javac.log" 2>&1; then
    echo "oracle: javac failed; see $WORK_DIR/javac.log" >&2
    cat "$WORK_DIR/javac.log" >&2
    exit 1
fi

# Run TestRig. -tree prints the parse tree as an S-expression;
# -tokens prints the token stream. We capture stderr separately so
# parse errors flagged by ANTLR4's recognizer surface as exit-2 below.
TESTRIG_FLAG="-tree"
[ "$MODE" = "tokens" ] && TESTRIG_FLAG="-tokens"

set +e
java -cp "$JAR_PATH:$WORK_DIR" org.antlr.v4.gui.TestRig \
    "$GRAMMAR_NAME" "$START_RULE" "$TESTRIG_FLAG" \
    >"$WORK_DIR/out.txt" 2>"$WORK_DIR/err.txt"
RC=$?
set -e

if [ -s "$WORK_DIR/err.txt" ]; then
    cat "$WORK_DIR/err.txt" >&2
fi

cat "$WORK_DIR/out.txt"

if [ $RC -ne 0 ]; then
    exit 1
fi
if [ -s "$WORK_DIR/err.txt" ]; then
    exit 2
fi
exit 0
