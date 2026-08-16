#!/usr/bin/env bash
# Check Gale's `\p{...}` tables against the published ANTLR4 jar, one whole
# property at a time and over the whole code-point space.
#
# For each property it takes the value list from the UCD's
# PropertyValueAliases.txt, asks `antlr4-property-oracle.sh` for the jar's own
# table, and has `check_unicode_properties.wado` compare it against what Gale
# expands `\p{PROPERTY=VALUE}` into.
#
# The jar carries a Unicode snapshot frozen when it was built (4.13.2 is
# Unicode 15.0.0, measured), so compare like-for-like:
#
#     scripts/regen-unicode-tables.sh 15.0.0   # match the jar
#     scripts/check-unicode-properties.sh
#     scripts/regen-unicode-tables.sh          # back to latest
#
# Skipping that step reports the Unicode versions' differences as failures.
#
# Usage: scripts/check-unicode-properties.sh [property...]   (default: all)
# Needs java, javac, network access, and a built `wado` (WADO env, default
# ../target/debug/wado).
set -euo pipefail
cd "$(dirname "$0")/.."

DEFAULT_PROPERTIES="gc sc blk bc lb WB SB GCB nt ea hst jt InSC dt vo"
PROPERTIES="${*:-$DEFAULT_PROPERTIES}"

WADO="${WADO:-../target/debug/wado}"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/gale"
mkdir -p "$CACHE_DIR"

TABLE_VERSION=$(sed -n 's/^pub global UNICODE_VERSION: String = "\([^"]*\)".*/\1/p' src/g4/unicode_tables.wado)
echo "checking tables generated from UCD $TABLE_VERSION" >&2

ALIASES="$CACHE_DIR/PropertyValueAliases-$TABLE_VERSION.txt"
if [ ! -f "$ALIASES" ]; then
    if [ "$TABLE_VERSION" = "" ]; then
        echo "check: cannot read UNICODE_VERSION from src/g4/unicode_tables.wado" >&2
        exit 1
    fi
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
    if ! scripts/antlr4-property-oracle.sh "$property" $values > "$WORK/table.tsv"; then
        echo "check: the oracle refused '$property'" >&2
        FAILED=$((FAILED + 1))
        continue
    fi
    if ! "$WADO" run --dir "$WORK" -- scripts/check_unicode_properties.wado "$property" table.tsv; then
        FAILED=$((FAILED + 1))
    fi
done

if [ "$FAILED" -gt 0 ]; then
    echo "check: $FAILED properties differ from the jar" >&2
    exit 1
fi
