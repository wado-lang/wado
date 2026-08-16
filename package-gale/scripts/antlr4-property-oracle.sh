#!/usr/bin/env bash
# Reconstruct the published ANTLR4 jar's own code-point table for one Unicode
# property, by running the jar as a black-box oracle. See the License hygiene
# section in package-gale/AGENTS.md: we may run the published jar to observe
# its behavior, but we may not read ANTLR4's implementation source.
#
# For `<property>` with values `v1 v2 ...` this synthesizes
#
#     lexer grammar PropOracle;
#     V0 : [\p{<property>=v1}]+ ;
#     ...
#
# feeds it every Unicode scalar in code-point order, and prints one
# `VALUE<TAB>START-END` line (hex) per maximal run. A `+` rule consumes a whole
# run, so the token stream is the jar's range table — an exact whole-space
# answer rather than a spot check. A run no value claims prints as `?`.
#
# The jar carries a Unicode snapshot frozen when it was built (4.13.2 is
# Unicode 15.0.0), so a diff against Gale's tables is only meaningful with
# Gale's tables regenerated from that same version:
#
#     scripts/regen-unicode-tables.sh 15.0.0
#
# Any difference that survives that is a Gale derivation bug rather than
# version drift. Compare over the scalars only: the oracle never sees a
# surrogate, and Gale drops them on the way in.
#
# Usage:
#   scripts/antlr4-property-oracle.sh <property> <value>...
#
# Exit codes:
#   0 = table printed
#   1 = invocation error (no values, missing JDK, jar unavailable)
#   2 = the jar rejected the property or one of its values

set -euo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=antlr4-jar.sh
. "$(dirname "$0")/antlr4-jar.sh"

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

# ANTLR4 answers a narrower `Property=Value` surface than the UCD names: group
# letters (`gc=L`) and values with no code points (`sc=Hrkt`) are rejected even
# though the property itself is fine. Drop exactly what it names and retry, so
# one refused value does not cost the whole property — and say which, since
# that boundary is the compatibility contract.
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

cp scripts/PropertyOracle.java "$WORK_DIR/"
if ! (cd "$WORK_DIR" && javac -cp "$JAR_PATH" ./*.java) > "$WORK_DIR/javac.log" 2>&1; then
    echo "oracle: javac failed:" >&2
    sed 's/^/oracle:   /' "$WORK_DIR/javac.log" >&2
    exit 1
fi

# `-Xss` because the run holds every scalar in one string; the default stack is
# fine but the default heap is not on a 32-bit-ish default sizing.
java -Xmx2g -cp "$JAR_PATH:$WORK_DIR" PropertyOracle > "$WORK_DIR/table.tsv"

# Strip the `V_` that made each value a legal rule name.
sed 's/^V_//' "$WORK_DIR/table.tsv"
