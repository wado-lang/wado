#!/usr/bin/env bash
# Compile a Wado service and transpile it into gen/ for the Worker.
#
# Usage: ./build.sh [program.wado]      (default: ../example/http_bin.wado)
set -e -o pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
PROGRAM="${1:-$ROOT/example/http_bin.wado}"
JCO="$ROOT/scripts/jco/node_modules/.bin/jco"

[ -x "$JCO" ] || { echo "jco missing. Run: mise run jco-deps" >&2; exit 1; }

WASM="$(mktemp -d)/service.wasm"
# V8 has no wide-arithmetic, which float formatting emits.
cargo run -q -p wado-cli --manifest-path "$ROOT/Cargo.toml" -- \
  compile --world wasi:http/service -f no-wide-arithmetic -Os -o "$WASM" "$PROGRAM"

rm -rf "$HERE/gen"
# `--instantiation` because a Worker rejects the top-level await jco's default
# output initializes with, and cannot fetch a core module by URL.
"$JCO" transpile "$WASM" -o "$HERE/gen" --name service \
  --instantiation async \
  --no-wasi-shim \
  --map "wasi:cli/*=./shims/cli.js#*" \
  --map "wasi:clocks/*=./shims/clocks.js#*" \
  --map "wasi:http/*=./shims/http.js#*"

echo "built $(basename "$PROGRAM") → $HERE/gen"
