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

echo "==> installing jco + esbuild (if needed)"
{ [ -d "$JCO/node_modules/@bytecodealliance/jco-transpile" ] && [ -d "$JCO/node_modules/esbuild" ]; } || (cd "$JCO" && npm install)

echo "==> bundling jco transpileBytes for the browser"
mkdir -p "$WEB/vendor"
node "$WEB/build-jco.mjs"

echo "==> staging jco runtime assets"
cp "$JCO_VENDOR"/js-component-bindgen-component.core*.wasm "$WEB/vendor/"
cp "$JCO_VENDOR"/wasm-tools.core*.wasm "$WEB/vendor/"
cp "$JCO/missing-intrinsics.js" "$WEB/vendor/"
# Stage with a .js extension: some static servers (python http.server) don't map
# .mjs to a JavaScript MIME type, which blocks `import` of an ES module.
cp "$JCO/postprocess.mjs" "$WEB/vendor/postprocess.js"

echo "==> done. Serve wado-playground/web/ over HTTP and open index.html"
ls -la "$WEB/wado-playground.wasm" "$WEB/vendor/"
