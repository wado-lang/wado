#!/usr/bin/env bash
# The llama.cpp Vulkan arm, named $1, on the GGUF at $2, continuing the prompt
# the generate.json at $3 records, with the experts of the first $4 layers on
# the CPU. Run from the benchmark directory once the build-llama-cpp task has
# built it. Without llama.cpp's release the arm is skipped.
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
build/llama_cpp "$2" "$llama" "$3" -1 "$cores" "$4"
