#!/usr/bin/env bash
# Regenerate src/g4/unicode_tables.wado from the Unicode Character Database.
#
# Gale expands `\p{...}` / `\P{...}` into code-point ranges at grammar-parse
# time. `\P` is the complement of `\p`, so an approximate table does not merely
# miss characters — it admits them. The table is therefore generated from the
# UCD rather than hand-maintained. General categories, scripts, blocks and
# binary properties are covered; anything else is rejected loudly.
#
# Usage: scripts/regen-unicode-tables.sh [version]   (default: latest)
# Needs network access to unicode.org and a built `wado` (WADO env, default
# ../target/debug/wado).
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="${1:-latest}"
BASE="https://www.unicode.org/Public/UCD/${VERSION}/ucd"
if [ "$VERSION" != "latest" ]; then
    BASE="https://www.unicode.org/Public/${VERSION}/ucd"
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

for f in UnicodeData.txt Scripts.txt Blocks.txt PropList.txt DerivedCoreProperties.txt \
    PropertyAliases.txt PropertyValueAliases.txt ReadMe.txt; do
    curl -fsS -o "$WORK/$f" "$BASE/$f"
done
# Emoji properties (`Extended_Pictographic`, `Emoji`, …) ship in their own file.
curl -fsS -o "$WORK/emoji-data.txt" "$BASE/emoji/emoji-data.txt"

# "for Version 17.0.0 of the Unicode Standard."
RESOLVED="$(grep -oE 'Version [0-9]+\.[0-9]+\.[0-9]+ of the Unicode Standard' "$WORK/ReadMe.txt" |
    head -1 | sed -E 's/Version ([0-9.]+) .*/\1/')"
if [ -z "$RESOLVED" ]; then
    echo "could not read the UCD version from ReadMe.txt" >&2
    exit 1
fi

WADO="${WADO:-../target/debug/wado}"
"$WADO" run --dir "$WORK" -- scripts/gen_unicode_tables.wado "$RESOLVED" \
    > src/g4/unicode_tables.wado
echo "regenerated src/g4/unicode_tables.wado from UCD $RESOLVED"
