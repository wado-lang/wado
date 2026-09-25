#!/usr/bin/env bash
# Compile src/main.wado and bundle it with jco's browser shims into build/.
set -e -o pipefail

if ! command -v node >/dev/null; then
  [ -z "${WADO_MISE_EXEC:-}" ] || { echo "error: node not found, even under mise" >&2; exit 1; }
  WADO_MISE_EXEC=1 exec mise exec -- "$0" "$@"
fi

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
BUILD="$HERE/build"

rm -rf "$BUILD"
mkdir -p "$BUILD"
# V8 has no wide-arithmetic, which float formatting emits.
cargo run -q -p wado-cli --manifest-path "$ROOT/Cargo.toml" -- \
  compile -f no-wide-arithmetic -Os -o "$BUILD/main.wasm" "$HERE/src/main.wado"
node "$ROOT/scripts/jco/transpile-released.mjs" "$BUILD/main.wasm" "$BUILD"
echo 'import { run } from "./main.js"; await run.run();' > "$BUILD/boot.js"
# The bundle sits beside the core modules jco fetches relative to it. jco reads
# them with node:fs only under Node, so the browser never loads that import.
"$ROOT/scripts/jco/node_modules/.bin/esbuild" "$BUILD/boot.js" --bundle --format=esm \
  --platform=browser --external:'node:*' --outfile="$BUILD/app.js" --log-level=error
