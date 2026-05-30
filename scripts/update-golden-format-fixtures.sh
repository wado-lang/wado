#!/usr/bin/env bash
set -euo pipefail

# Regenerate format fixture golden files:
#   - Formatted (clean) version of each dirty fixture
#   - Compiler phase outputs (NIR, lower, optimize, WIR, WAT) for compilable fixtures
#
# Every file is overwritten in place; stale files (no matching dirty fixture)
# are pruned only after all outputs have been regenerated, so the golden set is
# never wiped mid-run.
#
# Expects binaries to be pre-built (on-task-done builds them before calling this).

FIXTURES_DIR="wado-compiler/tests/format.fixtures"
GOLDEN_DIR="wado-compiler/tests/generated/format.fixtures"
mkdir -p "$GOLDEN_DIR"

WADO="./target/debug/wado"
DUMP="./target/debug/wado-dev-tools"

# Generate formatted (clean) versions, collecting the live fixture names.
declare -A live_names=()
for f in "$FIXTURES_DIR"/*.dirty.wado; do
  name=$(basename "$f" .dirty.wado)
  clean="$GOLDEN_DIR/$name.clean.wado"
  cp "$f" "$clean"
  "$WADO" format -w "$clean"
  echo "  Generated $clean"
  live_names["$name"]=1
done

# Prune stale clean files whose dirty fixture no longer exists. golden-dump
# prunes the compiler-phase outputs itself; the clean files are produced here,
# so we prune them here too — after regeneration, never before.
for c in "$GOLDEN_DIR"/*.clean.wado; do
  [ -e "$c" ] || continue
  base=$(basename "$c" .clean.wado)
  if [ -z "${live_names[$base]:-}" ]; then
    rm -f "$c"
    echo "  Pruned $c"
  fi
done

# Generate every compiler phase output in a single golden-dump invocation:
# each fixture is compiled once, all phases are emitted, and stale per-suffix
# outputs are pruned at the end.
# --skip-empty handles no_prelude files that produce empty output.
IN="$FIXTURES_DIR/{name}.dirty.wado"
$DUMP golden-dump --in "$IN" --skip-empty -O2 \
  --emit nir:"$GOLDEN_DIR/{name}.nir.wado" \
  --emit nir-lowered:"$GOLDEN_DIR/{name}.lower.wado" \
  --emit nir:"$GOLDEN_DIR/{name}.optimize.wado" \
  --emit wir:"$GOLDEN_DIR/{name}.wir.wado" \
  --emit wat:"$GOLDEN_DIR/{name}.wat"

echo "Golden format fixtures updated."
