#!/usr/bin/env bash
# Downloads llama.cpp's Vulkan release for this host, unmodified, and prints the
# directory holding its libraries, or nothing on a host it has no release for.
# The headers the release leaves out come from the source of the same tag,
# under `<directory>/include`. The release is the one llama_cpp.lock pins.
# Cached under ~/.cache/loam.
set -euo pipefail

source "$(dirname "$0")/llama_cpp.lock"
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)
        asset=llama-$TAG-bin-ubuntu-vulkan-x64
        sha=$UBUNTU_VULKAN_X64_SHA
        ;;
    *)
        exit 0
        ;;
esac

# Assembled beside the cache and renamed into it, so an interrupted run leaves
# nothing a later one would take for the release.
if command -v sha256sum > /dev/null; then sha256=(sha256sum); else sha256=(shasum -a 256); fi
dir="$HOME/.cache/loam/$asset"
if [ ! -d "$dir" ]; then
    part="$dir.part"
    rm -rf "$part" "$part.tgz" "$part.src.tgz"
    mkdir -p "$part/include"
    curl -fsSL -o "$part.tgz" "https://github.com/ggml-org/llama.cpp/releases/download/$TAG/$asset.tar.gz"
    echo "$sha  $part.tgz" | "${sha256[@]}" -c - >&2
    tar -xzf "$part.tgz" -C "$part" --strip-components 1
    curl -fsSL -o "$part.src.tgz" "https://github.com/ggml-org/llama.cpp/archive/refs/tags/$TAG.tar.gz"
    echo "$SOURCE_SHA  $part.src.tgz" | "${sha256[@]}" -c - >&2
    tar -xzf "$part.src.tgz" -C "$part/include" --strip-components 2 "llama.cpp-$TAG/include/llama.h"
    tar -xzf "$part.src.tgz" -C "$part/include" --strip-components 3 "llama.cpp-$TAG/ggml/include"
    rm "$part.tgz" "$part.src.tgz"
    mv "$part" "$dir"
fi
echo "$dir"
