#!/usr/bin/env bash
# Downloads GPT-2 (124M, MIT) from Hugging Face: the model the build read, whose
# weights gpt2.wado loads from where the build found them.
set -euo pipefail
cd "$(dirname "$0")"

REVISION=607a30d783dfa663caf39e06633721c8d4cfcd7e
curl -fL -o model.onnx "https://huggingface.co/openai-community/gpt2/resolve/$REVISION/onnx/decoder_model_merged.onnx"
if command -v sha256sum > /dev/null; then sha256=(sha256sum); else sha256=(shasum -a 256); fi
echo "e6fc046fe5a7cfeeb8bb3c7d4c1b6a8bd90ced7339a75d5567957e3bc9d48abe  model.onnx" | "${sha256[@]}" -c -
