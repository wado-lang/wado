#!/usr/bin/env bash
# Downloads GPT-2 (124M, MIT) from Hugging Face and splits it into the graph and
# the checkpoint gpt2.wado runs with. Set WADO to use another binary.
set -euo pipefail
cd "$(dirname "$0")"

REVISION=607a30d783dfa663caf39e06633721c8d4cfcd7e
curl -fL -o model.onnx "https://huggingface.co/openai-community/gpt2/resolve/$REVISION/onnx/decoder_model.onnx"
if command -v sha256sum > /dev/null; then sha256=(sha256sum); else sha256=(shasum -a 256); fi
echo "e3fc9615868ff8f5e0429b892a0f6ca692784ba6c4ca31c4e9ee8218e7cce34f  model.onnx" | "${sha256[@]}" -c -

"${WADO:-wado}" run ../../tools/onnx_split.wado -- model.onnx gpt2
# The build reads only the checkpoint's header: its length, then the JSON.
head -c "$((8 + $(od -An -t u8 -N 8 gpt2.safetensors)))" gpt2.safetensors > gpt2-header.safetensors
