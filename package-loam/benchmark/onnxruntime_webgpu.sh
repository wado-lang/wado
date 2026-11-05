#!/usr/bin/env bash
# The onnxruntime WebGPU arm for the example in $1, where `mise run
# loam-oracle-deps` has installed onnxruntime-node.
set -e
echo "=== onnxruntime (WebGPU) ==="
if [ ! -d "$(dirname "$0")/../node_modules/onnxruntime-node" ]; then
    echo "SKIP: run mise run loam-oracle-deps first"
    exit 0
fi
node "$(dirname "$0")/onnxruntime_webgpu.mjs" "$1"
