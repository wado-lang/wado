#!/usr/bin/env bash
# Downloads llama.cpp's Vulkan release for this host, unmodified, and prints the
# directory holding its libraries. The headers the release leaves out come from
# the source of the same tag, under `<directory>/include`. Cached under
# ~/.cache/loam.
set -euo pipefail

TAG=b11846
SOURCE_SHA=137c4a1fb0790370d528f4b52e9c9c53eb9273ad9ab99711329b13f51d02baad
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)
        asset=llama-$TAG-bin-ubuntu-vulkan-x64
        sha=92d10e8f3a4370988b6b7f7aa23647fb74dc3e59f8a9d2d407cb381c4932fc7a
        ;;
    *)
        echo "no llama.cpp $TAG Vulkan release for $(uname -s)-$(uname -m)" >&2
        exit 1
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
