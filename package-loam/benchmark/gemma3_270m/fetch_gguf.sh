#!/usr/bin/env bash
# Downloads Gemma 3 (270M, instruction-tuned) as GGUF and prints its path:
# F16 by default, the precision nearest the checkpoint the Loam arms run, or
# the precision the argument names, Q4_0 or Q4_1, for the q4 exports' table.
# Google ships no F16 GGUF and ggml-org only Q8_0, so these are unsloth's
# conversions. Cached under ~/.cache/loam.
set -euo pipefail

REVISION=c90975dbd40c0c7b275fefaae758c3415c906238
case "${1:-F16}" in
    F16) SHA=140cb395a3a3dcdb3de66e44c60d7265286c9d49f951c1899ec0d8c1f16b2feb ;;
    Q4_0) SHA=e479ea2962bdcdc7e6321b91148b9ac2f516f649e0921412561d4936aadef158 ;;
    Q4_1) SHA=bcb89e49851a86649fcd738caebcbe277fe1026e4e274ade27af9ce582d2d79b ;;
    *)
        echo "no GGUF of precision $1" >&2
        exit 2
        ;;
esac
FILE=gemma-3-270m-it-${1:-F16}.gguf

path="$HOME/.cache/loam/$FILE"
if [ ! -f "$path" ]; then
    mkdir -p "$(dirname "$path")"
    curl -fsSL -o "$path.part" "https://huggingface.co/unsloth/gemma-3-270m-it-GGUF/resolve/$REVISION/$FILE"
    if command -v sha256sum > /dev/null; then sha256=(sha256sum); else sha256=(shasum -a 256); fi
    echo "$SHA  $path.part" | "${sha256[@]}" -c - >&2
    mv "$path.part" "$path"
fi
echo "$path"
