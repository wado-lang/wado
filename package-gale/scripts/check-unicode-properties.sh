#!/usr/bin/env bash
# Check Gale's `\p{...}` tables against the published ANTLR4 jar over the whole
# code-point space, one property at a time: the value list comes from the UCD's
# PropertyValueAliases.txt, the jar's table from `antlr4-property-oracle.sh`,
# and the diff from `check_unicode_properties.wado`.
#
# The jar's Unicode snapshot is frozen at its build (4.13.2 is 15.0.0), so
# compare like-for-like or every Unicode change since reads as a failure:
#
#     scripts/regen-unicode-tables.sh 15.0.0   # match the jar
#     scripts/check-unicode-properties.sh
#     scripts/regen-unicode-tables.sh          # back to latest
#
# Usage: scripts/check-unicode-properties.sh [property...]   (default: all)
# Needs java, javac, network access, and a built `wado` (WADO env, default
# ../target/debug/wado).
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

DEFAULT_PROPERTIES="gc sc blk bc lb WB SB GCB nt ea hst jt InSC dt vo"
PROPERTIES="${*:-$DEFAULT_PROPERTIES}"

WADO="${WADO:-../target/debug/wado}"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/gale"
mkdir -p "$CACHE_DIR"

TABLE_VERSION=$(sed -n 's/^pub global UNICODE_VERSION: String = "\([^"]*\)".*/\1/p' src/g4/unicode_tables.wado)
if [ -z "$TABLE_VERSION" ]; then
    echo "check: cannot read UNICODE_VERSION from src/g4/unicode_tables.wado" >&2
    exit 1
fi
echo "checking tables generated from UCD $TABLE_VERSION" >&2

ALIASES="$CACHE_DIR/PropertyValueAliases-$TABLE_VERSION.txt"
if [ ! -f "$ALIASES" ]; then
    curl -fsS -o "$ALIASES" "https://www.unicode.org/Public/$TABLE_VERSION/ucd/PropertyValueAliases.txt"
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

FAILED=0
for property in $PROPERTIES; do
    # PropertyValueAliases.txt is `PROPERTY ; SHORT ; LONG [; …]`; the short
    # name is enough to name a value, and every value has one.
    values=$(awk -F';' -v p="$property" '
        { gsub(/^[ \t]+|[ \t]+$/, "", $1); gsub(/^[ \t]+|[ \t]+$/, "", $2) }
        $1 == p && $2 != "" && $2 !~ /^#/ { print $2 }
    ' "$ALIASES" | sort -u)
    if [ -z "$values" ]; then
        echo "check: no values for '$property' in $ALIASES" >&2
        FAILED=$((FAILED + 1))
        continue
    fi
    # shellcheck disable=SC2086
    if ! "$SCRIPT_DIR/antlr4-property-oracle.sh" "$property" $values > "$WORK/table.tsv"; then
        echo "check: the oracle refused '$property'" >&2
        FAILED=$((FAILED + 1))
        continue
    fi
    if ! "$WADO" run --dir "$WORK" -- "$SCRIPT_DIR/check_unicode_properties.wado" "$property" table.tsv; then
        FAILED=$((FAILED + 1))
    fi
done

if [ "$FAILED" -gt 0 ]; then
    echo "check: $FAILED properties differ from the jar" >&2
    exit 1
fi
