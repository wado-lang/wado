#!/usr/bin/env bash
# Downloads gpt-oss-20b as ggml-org's GGUF and prints its path: MXFP4, the
# experts' precision as OpenAI released them. Cached under ~/.cache/loam.
set -euo pipefail

REVISION=41e8a94d15d62070b36cf4553a0250717ecf46b8
SHA=27cd6c432c7672cb812a92f611cf3ba7bbc35928262bb1e1253ff4ee6ae35901
FILE=gpt-oss-20b-MXFP4.gguf

path="$HOME/.cache/loam/$FILE"
if [ ! -f "$path" ]; then
    mkdir -p "$(dirname "$path")"
    curl -fsSL -o "$path.part" "https://huggingface.co/ggml-org/gpt-oss-20b-GGUF/resolve/$REVISION/$FILE"
    if command -v sha256sum > /dev/null; then sha256=(sha256sum); else sha256=(shasum -a 256); fi
    echo "$SHA  $path.part" | "${sha256[@]}" -c - >&2
    mv "$path.part" "$path"
fi
echo "$path"
