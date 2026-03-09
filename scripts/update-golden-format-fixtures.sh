#!/usr/bin/env bash
set -euo pipefail

# Regenerate format fixture golden files:
#   - Formatted (clean) version of each dirty fixture
#   - Compiler phase outputs (TIR, lower, optimize, WIR, WAT) for compilable fixtures

cargo build --bin wado --quiet

FIXTURES_DIR="wado-compiler/tests/format.fixtures"
GOLDEN_DIR="wado-compiler/tests/format.fixtures.golden"
mkdir -p "$GOLDEN_DIR"

tmpfile=$(mktemp)
trap 'rm -f "$tmpfile"' EXIT

# Run a dump command, writing to a temp file first.
# Only overwrites the target if the command succeeds and produces non-empty output.
run_dump() {
  local target="$1"
  shift
  if "$@" > "$tmpfile" 2>/dev/null && [ -s "$tmpfile" ]; then
    mv "$tmpfile" "$target"
    tmpfile=$(mktemp)
  else
    echo "WARNING: failed to generate $target" >&2
    exit 1
  fi
}

for f in "$FIXTURES_DIR"/*.dirty.wado; do
  name=$(basename "$f" .dirty.wado)
  clean="$GOLDEN_DIR/$name.clean.wado"

  # Generate formatted (clean) version
  cp "$f" "$clean"
  cargo run --bin wado --quiet -- format -w "$clean"

  # Skip compiler phase outputs for no_prelude files
  if grep -q '^#!\[no_prelude\]' "$f"; then continue; fi

  # Generate compiler phase outputs
  run_dump "$GOLDEN_DIR/$name.tir.wado"      cargo run --bin wado --quiet -- dump --tir "$f"
  run_dump "$GOLDEN_DIR/$name.lower.wado"    cargo run --bin wado --quiet -- dump --tir-lowered "$f"
  run_dump "$GOLDEN_DIR/$name.optimize.wado" cargo run --bin wado --quiet -- dump --tir -O2 "$f"
  run_dump "$GOLDEN_DIR/$name.wir.wado"      cargo run --bin wado --quiet -- dump --wir -O2 "$f"
  run_dump "$GOLDEN_DIR/$name.wat"           cargo run --bin wado --quiet -- compile --wat-to-stdout "$f"
done

echo "Golden format fixtures updated."
