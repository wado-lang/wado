#!/usr/bin/env bash
# Downloads Gemma 3 (270M, instruction-tuned) as onnx-community exports it into
# build/: the weights gemma3.wado, fp16.wado, q4.wado and q4f16.wado load, and the models and
# tokenizer the oracle scripts run.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p build

REPO=https://huggingface.co/onnx-community/gemma-3-270m-it-ONNX/resolve/2dbbfdb1b59bd034eb959428c6a7da9dd7ea27f0
curl -fL -o build/model.onnx "$REPO/onnx/model.onnx"
curl -fL -o build/model.onnx_data "$REPO/onnx/model.onnx_data"
curl -fL -o build/model_fp16.onnx "$REPO/onnx/model_fp16.onnx"
curl -fL -o build/model_fp16.onnx_data "$REPO/onnx/model_fp16.onnx_data"
curl -fL -o build/model_q4.onnx "$REPO/onnx/model_q4.onnx"
curl -fL -o build/model_q4.onnx_data "$REPO/onnx/model_q4.onnx_data"
curl -fL -o build/model_q4f16.onnx "$REPO/onnx/model_q4f16.onnx"
curl -fL -o build/model_q4f16.onnx_data "$REPO/onnx/model_q4f16.onnx_data"
curl -fL -o build/tokenizer.json "$REPO/tokenizer.json"
curl -fL -o build/tokenizer_config.json "$REPO/tokenizer_config.json"
if command -v sha256sum > /dev/null; then sha256=(sha256sum); else sha256=(shasum -a 256); fi
"${sha256[@]}" -c - << 'EOF'
56adf096be205ed53d9a1c59180ab2bdd47649a385705c7ab6a64f12d93e06b4  build/model.onnx
21ceb4e7effff33b74d22bb5587c57fd1dc611b3da84abf98145f48e6e5acebd  build/model.onnx_data
9e48315be771ed91edc2645283219a80d9fb1645131ea9c2d63ace9c8e570f13  build/model_fp16.onnx
df1de41111967fd1399bce6e7891a92ae26db2c9855b300c0b24dae4fcd4d508  build/model_fp16.onnx_data
cb3615746a988839308507933d4e0501ff695e747a0a0ce15c0ef53083f8ec54  build/model_q4.onnx
25c4a14af32fc1011a90504076024b0e1f65f915e9690b44a5484200d2e20851  build/model_q4.onnx_data
01d620bf3c2c1d5b843a5c92a4456ce1f4c63f6ceb814da26a3420e8aad2b4a4  build/model_q4f16.onnx
23088037c5972441c6a6033f5fdcca41c96bfea9b0bf7e1abd6038ba42e45287  build/model_q4f16.onnx_data
55da1312bdf1d7d8fe8d9d1b3eed04086261149e6034e0ac3f8c633b67f5aac8  build/tokenizer.json
8ae5c9297e0ca85077c0f4952f85251b4773af7abc4673b62b54bc0b609a9667  build/tokenizer_config.json
EOF
