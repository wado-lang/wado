#!/usr/bin/env bash
# Downloads GPT-2 (124M, MIT) from Hugging Face: the model the build read, whose
# weights gpt2.wado loads from where the build found them.
set -euo pipefail
cd "$(dirname "$0")"

REVISION=607a30d783dfa663caf39e06633721c8d4cfcd7e
curl -fL -o model.onnx "https://huggingface.co/openai-community/gpt2/resolve/$REVISION/onnx/decoder_model.onnx"
if command -v sha256sum > /dev/null; then sha256=(sha256sum); else sha256=(shasum -a 256); fi
echo "e3fc9615868ff8f5e0429b892a0f6ca692784ba6c4ca31c4e9ee8218e7cce34f  model.onnx" | "${sha256[@]}" -c -
