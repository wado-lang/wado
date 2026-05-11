#!/usr/bin/env bash
set -euo pipefail

# Regenerate format fixture golden files:
#   - Formatted (clean) version of each dirty fixture
#   - Compiler phase outputs (NIR, lower, optimize, WIR, WAT) for compilable fixtures
#
# Expects binaries to be pre-built (on-task-done builds them before calling this).

FIXTURES_DIR="wado-compiler/tests/format.fixtures"
GOLDEN_DIR="wado-compiler/tests/format.fixtures.golden"
mkdir -p "$GOLDEN_DIR"

WADO="./target/debug/wado"
DUMP="./target/debug/wado-dev-tools"

# Generate formatted (clean) versions
for f in "$FIXTURES_DIR"/*.dirty.wado; do
  name=$(basename "$f" .dirty.wado)
  clean="$GOLDEN_DIR/$name.clean.wado"
  cp "$f" "$clean"
  "$WADO" format -w "$clean"
  echo "  Generated $clean"
done

# Generate compiler phase outputs via golden-dump
# --skip-empty handles no_prelude files that produce empty output
IN="$FIXTURES_DIR/{name}.dirty.wado"

$DUMP golden-dump --in "$IN" --out "$GOLDEN_DIR/{name}.nir.wado"      --phase nir         --skip-empty
$DUMP golden-dump --in "$IN" --out "$GOLDEN_DIR/{name}.lower.wado"    --phase nir-lowered --skip-empty
$DUMP golden-dump --in "$IN" --out "$GOLDEN_DIR/{name}.optimize.wado" --phase nir         -O2 --skip-empty
$DUMP golden-dump --in "$IN" --out "$GOLDEN_DIR/{name}.wir.wado"      --phase wir         -O2 --skip-empty
$DUMP golden-dump --in "$IN" --out "$GOLDEN_DIR/{name}.wat"           --phase wat         -O2 --skip-empty

echo "Golden format fixtures updated."
