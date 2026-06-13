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

echo
echo "###### collator + normalizer marker-dedup demo ######"

echo "== generate blobs: coll (collator+NFD), norm (all), shared (collator+all) =="
( cd datagen && cargo run --release -- coll ../coll.blob \
              && cargo run --release -- norm ../norm.blob \
              && cargo run --release -- shared ../shared.blob )

echo "== dedup math =="
A=$(stat -c%s coll.blob); B=$(stat -c%s norm.blob); AB=$(stat -c%s shared.blob)
echo "  coll(A)=$A  norm(B)=$B  shared(AB)=$AB  ->  deduped NFD = A+B-AB = $((A+B-AB)) bytes"

echo "== build data-free collator & normalizer feature components + shared data =="
( cd collator && cargo build --release )
( cd normalizer && cargo build --release )
cp shared.blob data-cn/shared.blob
( cd data-cn && cargo build --release )
wasm-tools component new collator/target/wasm32-unknown-unknown/release/cn_collator.wasm -o collator-feature.wasm
wasm-tools component new normalizer/target/wasm32-unknown-unknown/release/cn_normalizer.wasm -o normalizer-feature.wasm
wasm-tools component new data-cn/target/wasm32-unknown-unknown/release/cn_data.wasm -o cn-data.wasm

echo "== compose each feature with the ONE shared data component =="
wasm-tools compose collator-feature.wasm -d cn-data.wasm -o collator-composed.wasm
wasm-tools compose normalizer-feature.wasm -d cn-data.wasm -o normalizer-composed.wasm
wasm-tools validate collator-composed.wasm && wasm-tools validate normalizer-composed.wasm

echo "== sizes =="
ls -l collator-feature.wasm normalizer-feature.wasm cn-data.wasm collator-composed.wasm normalizer-composed.wasm

echo "== runtime check (both features off one shared blob) =="
( cd runtime-check-cn && cargo run --release )

echo
echo "###### negative control: casemap + properties + segmenter ######"
echo "== generate per-feature blobs and their union =="
( cd datagen && for s in casemap properties segmenter csp; do cargo run --release -- $s ../$s.blob; done )
C=$(stat -c%s casemap.blob); P=$(stat -c%s properties.blob)
S=$(stat -c%s segmenter.blob); U=$(stat -c%s csp.blob)
echo "  casemap=$C properties=$P segmenter=$S  sum=$((C+P+S))  union=$U"
echo "  dedup = sum - union = $((C+P+S-U)) bytes  (≈0: these features share no markers)"
