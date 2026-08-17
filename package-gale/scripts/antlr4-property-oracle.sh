#!/usr/bin/env bash
# Reconstruct the published ANTLR4 jar's own code-point table for one Unicode
# property, by running the jar as a black-box oracle (License hygiene in
# package-gale/AGENTS.md: run the jar, never read its source).
#
# Synthesizes one `V_<value> : [\p{<property>=<value>}]+ ;` rule per value,
# feeds it every Unicode scalar in code-point order, and prints one
# `VALUE<TAB>START-END` line (hex) per maximal run — a `+` rule consumes a
# whole run, so the token stream is the jar's range table. A run no value
# claims prints as `?`.
#
# Callers want `check-unicode-properties.sh`, which drives this per property
# and diffs the result; "Oracling the Unicode property tables" in
# antlr4-compatibility.md covers why the diff needs matching Unicode versions.
#
# Usage: scripts/antlr4-property-oracle.sh <property> <value>...
#
# Exit codes:
#   0 = table printed
#   1 = invocation error (no values, missing JDK, jar unavailable)
#   2 = the jar rejected the property or one of its values

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

# shellcheck source=antlr4-jar.sh
. "$SCRIPT_DIR/antlr4-jar.sh"

if [ $# -lt 2 ]; then
    echo "Usage: $(basename "$0") <property> <value>..." >&2
    exit 1
fi
PROPERTY="$1"
shift

ensure_antlr4_jar

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

VALUES="$*"

write_grammar() {
    echo "lexer grammar PropOracle;" > "$WORK_DIR/PropOracle.g4"
    for value in $VALUES; do
        echo "V_$value : [\\p{$PROPERTY=$value}]+ ;" >> "$WORK_DIR/PropOracle.g4"
    done
}

# ANTLR4's `Property=Value` surface is narrower than the UCD's: group letters
# (`gc=L`) and empty values (`sc=Hrkt`) are rejected. Drop exactly what it
# names and retry, saying which — that boundary is the contract.
drop_rejected() {
    local rejected
    rejected=$(sed -nE 's/.*PropOracle\.g4:[0-9]+:[0-9]+: invalid escape sequence \\p\{[^=]*=([^}]*)\}.*/\1/p' \
        "$WORK_DIR/antlr.log" | sort -u)
    [ -n "$rejected" ] || return 1
    local kept="" value
    for value in $VALUES; do
        if printf '%s\n' "$rejected" | grep -qx "$value"; then
            echo "oracle: the jar rejects $PROPERTY=$value" >&2
        else
            kept="$kept $value"
        fi
    done
    VALUES="${kept# }"
    [ -n "$VALUES" ]
}

generate() {
    write_grammar
    java -jar "$JAR_PATH" -Dlanguage=Java -no-listener -o "$WORK_DIR" \
        "$WORK_DIR/PropOracle.g4" > "$WORK_DIR/antlr.log" 2>&1
}

if ! generate; then
    if ! drop_rejected || ! generate; then
        echo "oracle: the jar rejected $PROPERTY:" >&2
        sed 's/^/oracle:   /' "$WORK_DIR/antlr.log" >&2
        exit 2
    fi
fi

cp "$SCRIPT_DIR/PropertyOracle.java" "$WORK_DIR/"
if ! (cd "$WORK_DIR" && javac -cp "$JAR_PATH" ./*.java) > "$WORK_DIR/javac.log" 2>&1; then
    echo "oracle: javac failed:" >&2
    sed 's/^/oracle:   /' "$WORK_DIR/javac.log" >&2
    exit 1
fi

java -cp "$JAR_PATH:$WORK_DIR" PropertyOracle > "$WORK_DIR/table.tsv"

sed 's/^V_//' "$WORK_DIR/table.tsv"
