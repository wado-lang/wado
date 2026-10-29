#!/usr/bin/env bash
# Downloads Gemma 3 (270M, instruction-tuned) as onnx-community exports it into
# build/: the weights gemma3.wado loads, and the model and tokenizer the oracle
# scripts run.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p build

REPO=https://huggingface.co/onnx-community/gemma-3-270m-it-ONNX/resolve/2dbbfdb1b59bd034eb959428c6a7da9dd7ea27f0
curl -fL -o build/model.onnx "$REPO/onnx/model.onnx"
curl -fL -o build/model.onnx_data "$REPO/onnx/model.onnx_data"
curl -fL -o build/tokenizer.json "$REPO/tokenizer.json"
curl -fL -o build/tokenizer_config.json "$REPO/tokenizer_config.json"
if command -v sha256sum > /dev/null; then sha256=(sha256sum); else sha256=(shasum -a 256); fi
"${sha256[@]}" -c - << 'EOF'
56adf096be205ed53d9a1c59180ab2bdd47649a385705c7ab6a64f12d93e06b4  build/model.onnx
21ceb4e7effff33b74d22bb5587c57fd1dc611b3da84abf98145f48e6e5acebd  build/model.onnx_data
55da1312bdf1d7d8fe8d9d1b3eed04086261149e6034e0ac3f8c633b67f5aac8  build/tokenizer.json
8ae5c9297e0ca85077c0f4952f85251b4773af7abc4673b62b54bc0b609a9667  build/tokenizer_config.json
EOF
