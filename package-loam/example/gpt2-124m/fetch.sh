#!/usr/bin/env bash
# Downloads GPT-2 (124M, MIT) from Hugging Face and splits it into the graph
# and the checkpoint gpt2.wado builds from. Set WADO to use another binary.
set -euo pipefail
cd "$(dirname "$0")"

REVISION=607a30d783dfa663caf39e06633721c8d4cfcd7e
BASE="https://huggingface.co/openai-community/gpt2/resolve/$REVISION"
curl -fL -o model.onnx "$BASE/onnx/decoder_model.onnx"
curl -fL -o vocab.json "$BASE/vocab.json"
curl -fL -o merges.txt "$BASE/merges.txt"
if command -v sha256sum > /dev/null; then sha256=(sha256sum); else sha256=(shasum -a 256); fi
"${sha256[@]}" -c - <<'EOF'
e3fc9615868ff8f5e0429b892a0f6ca692784ba6c4ca31c4e9ee8218e7cce34f  model.onnx
196139668be63f3b5d6574427317ae82f612a97c5d1cdaf36ed2256dbf636783  vocab.json
1ce1664773c50f3e0cc8842619a93edc4624525b728b188a9e0be33b7726adc5  merges.txt
EOF

"${WADO:-wado}" run ../../tools/onnx_split.wado -- model.onnx gpt2
