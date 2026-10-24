#!/usr/bin/env bash
# Downloads Gemma 3 (270M, instruction-tuned) as GGUF in F16, the precision
# nearest the checkpoint the Loam arms run, and prints its path. Google ships
# no F16 GGUF and ggml-org only Q8_0, so this is unsloth's conversion. Cached
# under ~/.cache/loam.
set -euo pipefail

REVISION=c90975dbd40c0c7b275fefaae758c3415c906238
FILE=gemma-3-270m-it-F16.gguf
SHA=140cb395a3a3dcdb3de66e44c60d7265286c9d49f951c1899ec0d8c1f16b2feb

path="$HOME/.cache/loam/$FILE"
if [ ! -f "$path" ]; then
    mkdir -p "$(dirname "$path")"
    curl -fsSL -o "$path.part" "https://huggingface.co/unsloth/gemma-3-270m-it-GGUF/resolve/$REVISION/$FILE"
    if command -v sha256sum > /dev/null; then sha256=(sha256sum); else sha256=(shasum -a 256); fi
    echo "$SHA  $path.part" | "${sha256[@]}" -c - >&2
    mv "$path.part" "$path"
fi
echo "$path"
