#!/usr/bin/env bash
# Build every generated asset the in-browser playground needs. Idempotent.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WEB="$ROOT/wado-playground/web"
JCO="$ROOT/scripts/jco"
WASM_OPT="$JCO/node_modules/binaryen/bin/wasm-opt"
JCO_VENDOR="$JCO/node_modules/@bytecodealliance/jco-transpile/vendor"

cd "$ROOT"

echo "==> installing jco + esbuild + binaryen (if needed)"
{ [ -d "$JCO/node_modules/@bytecodealliance/jco-transpile" ] && [ -d "$JCO/node_modules/esbuild" ] && [ -x "$WASM_OPT" ]; } || (cd "$JCO" && npm install)

# The playground cdylib also bundles the wado-lsp engine (same wasm32-u-u
# module), so the browser needs no separate LSP build. The VS Code extension
# keeps its own wasip1 stdio build via `mise run build-vscode-lsp`.
#
# Build for size (-Os + LTO, mirroring wado-bundled-libm): the module is large
# and download-bound, and the browser tolerates a slightly slower compiler.
echo "==> compiling wado-playground (wasm32-unknown-unknown, release -Os + LTO)"
CARGO_PROFILE_RELEASE_OPT_LEVEL=s CARGO_PROFILE_RELEASE_LTO=true \
  cargo build --release -p wado-playground --target wasm32-unknown-unknown

echo "==> optimizing with wasm-opt -Os"
"$WASM_OPT" -Os --strip-debug -all \
  target/wasm32-unknown-unknown/release/wado_playground.wasm \
  -o "$WEB/wado-playground.wasm"

echo "==> bundling jco transpileBytes for the browser"
mkdir -p "$WEB/vendor"
node "$WEB/build-jco.mjs"

echo "==> collecting playground examples"
node "$WEB/build-examples.mjs"

echo "==> staging jco runtime assets"
cp "$JCO_VENDOR"/js-component-bindgen-component.core*.wasm "$WEB/vendor/"
cp "$JCO_VENDOR"/wasm-tools.core*.wasm "$WEB/vendor/"
cp "$JCO/missing-intrinsics.js" "$WEB/vendor/"
# Stage with a .js extension: some static servers (python http.server) don't map
# .mjs to a JavaScript MIME type, which blocks `import` of an ES module.
cp "$JCO/postprocess.mjs" "$WEB/vendor/postprocess.js"

echo "==> done. Serve wado-playground/web/ over HTTP and open index.html"
ls -la "$WEB/wado-playground.wasm" "$WEB/vendor/"
