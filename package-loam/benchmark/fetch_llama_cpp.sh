#!/usr/bin/env bash
# Downloads llama.cpp's release for this host, unmodified, as the `cpu` or the
# `vulkan` build, and prints the directory holding its libraries. The headers
# the release leaves out come from the source of the same tag, under
# `<directory>/include`. Cached under ~/.cache/loam.
#
#   fetch_llama_cpp.sh cpu|vulkan
set -euo pipefail

TAG=b11846
SOURCE_SHA=137c4a1fb0790370d528f4b52e9c9c53eb9273ad9ab99711329b13f51d02baad
case "$(uname -s)-$(uname -m)-${1:?cpu or vulkan}" in
    Linux-x86_64-cpu)
        asset=llama-$TAG-bin-ubuntu-x64
        sha=eb26d338963f04eda3f70f23ef4a8575d44bc643f58d26c2f51925a58fbd43ff
        ;;
    Linux-x86_64-vulkan)
        asset=llama-$TAG-bin-ubuntu-vulkan-x64
        sha=92d10e8f3a4370988b6b7f7aa23647fb74dc3e59f8a9d2d407cb381c4932fc7a
        ;;
    *)
        echo "no llama.cpp $TAG $1 release for $(uname -s)-$(uname -m)" >&2
        exit 1
        ;;
esac

if command -v sha256sum > /dev/null; then sha256=(sha256sum); else sha256=(shasum -a 256); fi
dir="$HOME/.cache/loam/$asset"
if [ ! -d "$dir/include" ]; then
    mkdir -p "$dir"
    tgz="$dir.tgz"
    curl -fsSL -o "$tgz" "https://github.com/ggml-org/llama.cpp/releases/download/$TAG/$asset.tar.gz"
    echo "$sha  $tgz" | "${sha256[@]}" -c - >&2
    tar -xzf "$tgz" -C "$dir" --strip-components 1
    rm "$tgz"
    src="$dir.src.tgz"
    curl -fsSL -o "$src" "https://github.com/ggml-org/llama.cpp/archive/refs/tags/$TAG.tar.gz"
    echo "$SOURCE_SHA  $src" | "${sha256[@]}" -c - >&2
    mkdir -p "$dir/include"
    tar -xzf "$src" -C "$dir/include" --strip-components 2 "llama.cpp-$TAG/include/llama.h"
    tar -xzf "$src" -C "$dir/include" --strip-components 3 "llama.cpp-$TAG/ggml/include"
    rm "$src"
fi
echo "$dir"
