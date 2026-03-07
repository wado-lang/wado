#!/usr/bin/env bash
set -euo pipefail

# Regenerate format fixture golden files:
#   - Formatted (clean) version of each dirty fixture
#   - Compiler phase outputs (TIR, lower, optimize, WIR, WAT) for compilable fixtures

cargo build --bin wado --quiet

FIXTURES_DIR="wado-compiler/tests/format.fixtures"
GOLDEN_DIR="wado-compiler/tests/format.fixtures.golden"
mkdir -p "$GOLDEN_DIR"

for f in "$FIXTURES_DIR"/*.dirty.wado; do
  name=$(basename "$f" .dirty.wado)
  clean="$GOLDEN_DIR/$name.clean.wado"

  # Generate formatted (clean) version
  cp "$f" "$clean"
  cargo run --bin wado --quiet -- format -w "$clean"

  # Skip compiler phase outputs for no_prelude files
  if grep -q '^#!\[no_prelude\]' "$f"; then continue; fi

  # Generate compiler phase outputs (ignore non-zero exit from warnings)
  cargo run --bin wado --quiet -- dump --tir --unparse "$f" > "$GOLDEN_DIR/$name.tir.wado" 2>/dev/null || true
  cargo run --bin wado --quiet -- dump --lower --unparse "$f" > "$GOLDEN_DIR/$name.lower.wado" 2>/dev/null || true
  cargo run --bin wado --quiet -- dump --optimize --unparse -O2 "$f" > "$GOLDEN_DIR/$name.optimize.wado" 2>/dev/null || true
  cargo run --bin wado --quiet -- dump --wir --unparse -O2 "$f" > "$GOLDEN_DIR/$name.wir.wado" 2>/dev/null || true
  cargo run --bin wado --quiet -- compile --wat-to-stdout "$f" > "$GOLDEN_DIR/$name.wat" 2>/dev/null || true
done

echo "Golden format fixtures updated."
