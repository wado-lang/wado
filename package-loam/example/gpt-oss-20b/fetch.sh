#!/usr/bin/env bash
# Downloads gpt-oss-20b as onnx-community exports it in q4f16 into build/: the
# graph, its seven data files, and the tokenizer.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p build

REPO=https://huggingface.co/onnx-community/gpt-oss-20b-ONNX/resolve/0cb2c910963421423a6011d51dd0fef9d6e99f36
for file in model_q4f16.onnx model_q4f16.onnx_data model_q4f16.onnx_data_{1..6}; do
  curl -fL -C - -o "build/$file" "$REPO/onnx/$file"
done
curl -fL -o build/tokenizer.json "$REPO/tokenizer.json"
curl -fL -o build/tokenizer_config.json "$REPO/tokenizer_config.json"
if command -v sha256sum > /dev/null; then sha256=(sha256sum); else sha256=(shasum -a 256); fi
"${sha256[@]}" -c - << 'EOF'
5834cda2698332a02b1451a2935d10e7484b871bfa30c3a8b62cb72cc74e6a83  build/model_q4f16.onnx
2fb5a8564a09f6521e51b06f29384439b1285ccf9166533fceb4ed6af206aa7a  build/model_q4f16.onnx_data
0d3370b50d98413348135365c925b6af46346171693041c115d96f79c3f4fe21  build/model_q4f16.onnx_data_1
ff26e086a3887371ac51a89aeac752cbe2dce20cec514a2190d54287d302b4d4  build/model_q4f16.onnx_data_2
d4c49f1cb2f941bc5d457546c0b35707b518e0401d4dd9716238f45b7d6ef235  build/model_q4f16.onnx_data_3
3619260fcfd26b367cd886f1d21a24332f111ded159ca388b7e0d8f1026b414e  build/model_q4f16.onnx_data_4
4848013f1019367d427e5802c58b0adeb4f5cae31e1ff639fa39f9e70ef6660c  build/model_q4f16.onnx_data_5
74f352935e9af66748ad0afe803aee88f94735865ac3a6ff3f3380885fb1c18c  build/model_q4f16.onnx_data_6
0614fe83cadab421296e664e1f48f4261fa8fef6e03e63bb75c20f38e37d07d3  build/tokenizer.json
EOF
