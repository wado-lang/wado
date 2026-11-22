#!/usr/bin/env bash
# The llama.cpp Vulkan arm, named $1, continuing the prompt the generate.json
# at $2 records, with the experts of the first $3 layers on the CPU, on the
# GGUF whose path the rest of the arguments print as a command. Run from the
# benchmark directory once the build-llama-cpp task has built it. Without
# llama.cpp's release the arm is skipped, and the GGUF is not fetched.
set -e
llama=$(./fetch_llama_cpp.sh)
if [ -z "$llama" ]; then
    exit 0
fi
name=$1 oracle=$2 cpu_experts=$3
shift 3
gguf=$("$@")
# llama.cpp's own tools run a thread per physical core.
cores=$(getconf _NPROCESSORS_ONLN)
if command -v lscpu > /dev/null; then
    cores=$(lscpu -p=core | grep -v '^#' | sort -u | wc -l)
fi
echo "=== llama.cpp (Vulkan $name) ==="
build/llama_cpp "$gguf" "$llama" "$oracle" -1 "$cores" "$cpu_experts"
