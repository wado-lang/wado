#!/usr/bin/env bash
# Build every generated asset the in-browser playground needs. Idempotent.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WEB="$ROOT/wado-playground/web"
JCO="$ROOT/scripts/jco"
WASM_OPT="$JCO/node_modules/binaryen/bin/wasm-opt"
JCO_VENDOR="$JCO/node_modules/@bytecodealliance/jco-transpile/vendor"

cd "$ROOT"

echo "==> compiling wado-playground (wasm32-unknown-unknown, release)"
cargo build --release -p wado-playground --target wasm32-unknown-unknown

echo "==> optimizing with wasm-opt -O2"
"$WASM_OPT" -O2 --strip-debug -all \
  target/wasm32-unknown-unknown/release/wado_playground.wasm \
  -o "$WEB/wado-playground.wasm"

echo "==> installing released jco (if needed)"
[ -d "$JCO/node_modules/@bytecodealliance/jco-transpile" ] || (cd "$JCO" && npm install)

echo "==> bundling jco transpileBytes for the browser"
mkdir -p "$WEB/vendor"
node "$WEB/build-jco.mjs"

echo "==> staging jco runtime assets"
cp "$JCO_VENDOR"/js-component-bindgen-component.core*.wasm "$WEB/vendor/"
cp "$JCO_VENDOR"/wasm-tools.core*.wasm "$WEB/vendor/"
cp "$JCO/missing-intrinsics.js" "$WEB/vendor/"

echo "==> done. Serve wado-playground/web/ over HTTP and open index.html"
ls -la "$WEB/wado-playground.wasm" "$WEB/vendor/"
