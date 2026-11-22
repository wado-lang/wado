#!/usr/bin/env bash
# Times llama.cpp's Vulkan release on gpt-oss-20b: a prompt of 13 tokens and 32
# generated, every layer on the device but the experts of the number of layers
# each argument gives, which run on the CPU so the rest fit the device.
#
#     gpt_oss_20b/llama_bench.sh 10 14 24
set -euo pipefail

cd "$(dirname "$0")/.."
llama=$(./fetch_llama_cpp.sh)
if [ -z "$llama" ]; then
    echo "SKIP: llama.cpp has no Vulkan release for this host"
    exit 0
fi
model=$(gpt_oss_20b/fetch_gguf.sh)
for layers in "$@"; do
    "$llama/llama-bench" -m "$model" -ngl 99 -ncmoe "$layers" -p 13 -n 32 -r 3
done
