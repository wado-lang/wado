#!/usr/bin/env bash
# The llama.cpp Vulkan arm on Gemma 3 (270M) as GGUF of the precision $1, as
# fetch_gguf.sh names it, run from the benchmark directory once the
# build-llama-cpp task has built it. Without llama.cpp's release the arm is
# skipped.
set -e
llama=$(./fetch_llama_cpp.sh)
if [ -z "$llama" ]; then
    exit 0
fi
# llama.cpp's own tools run a thread per physical core.
cores=$(getconf _NPROCESSORS_ONLN)
if command -v lscpu > /dev/null; then
    cores=$(lscpu -p=core | grep -v '^#' | sort -u | wc -l)
fi
echo "=== llama.cpp (Vulkan $1) ==="
build/llama_cpp "$(gemma3_270m/fetch_gguf.sh "$1")" "$llama" -1 "$cores"
