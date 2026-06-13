#!/usr/bin/env bash
# Reproduces the BDP (BlobDataProvider) separation experiment end-to-end:
#   1. datagen   -> casemap.blob          (sliced postcard data, the shared asset)
#   2. casemap   -> casemap-feature.wasm  (data-free feature component, imports `data`)
#   3. data      -> data-provider.wasm    (shared data component, exports `data`)
#   4. compose   -> composed.wasm         (feature + data, import-free)
#   5. runtime-check                      (runs both scenarios under wasmtime)
#
# Prereq: the icu4x source cache must be seeded (datagen fetches CLDR/Unicode
# from GitHub). See README.md "Generating the blob".
set -euo pipefail
cd "$(dirname "$0")"

echo "== [1/5] datagen: slice casemap markers into a postcard blob =="
( cd datagen && cargo run --release -- ../casemap.blob )

echo "== [2/5] build data-free casemap feature component =="
( cd casemap && cargo build --release )
wasm-tools component new \
  casemap/target/wasm32-unknown-unknown/release/bdp_casemap.wasm \
  -o casemap-feature.wasm
wasm-tools validate casemap-feature.wasm

echo "== [3/5] build shared data component (bakes the blob) =="
cp casemap.blob data/casemap.blob
( cd data && cargo build --release )
wasm-tools component new \
  data/target/wasm32-unknown-unknown/release/bdp_data.wasm \
  -o data-provider.wasm
wasm-tools validate data-provider.wasm

echo "== [4/5] compose feature + shared data into one import-free component =="
wasm-tools compose casemap-feature.wasm -d data-provider.wasm -o composed.wasm
wasm-tools validate composed.wasm

echo "== sizes =="
ls -l casemap.blob casemap-feature.wasm data-provider.wasm composed.wasm

echo "== [5/5] runtime check (both scenarios) =="
( cd runtime-check && cargo run --release )
