#!/usr/bin/env bash
# Downloads Microsoft's onnxruntime release for this host, unmodified, and
# prints the directory it unpacked to. The version is the one onnxruntime-node
# pins in package-loam/package.json, so the native and Node arms run one
# onnxruntime. Cached under ~/.cache/loam.
set -euo pipefail

VERSION=1.30.0
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)
        asset=onnxruntime-linux-x64-$VERSION
        sha=a5ed5a3cac51fbb2e90da632ae43d19212faaa20e76484e62bcb7c23ddb3b3fd
        ;;
    Linux-aarch64)
        asset=onnxruntime-linux-aarch64-$VERSION
        sha=e16a27a8ed330bbc698df7330b0cf56e722f354e3bcc92118682c74ef3c3e3da
        ;;
    Darwin-arm64)
        asset=onnxruntime-osx-arm64-$VERSION
        sha=6ebb5062a934537c352937821f9fe9718e7de1a2db1122a93dd363ffd53a7012
        ;;
    *)
        echo "no onnxruntime $VERSION release for $(uname -s)-$(uname -m)" >&2
        exit 1
        ;;
esac

cache="$HOME/.cache/loam"
if [ ! -d "$cache/$asset" ]; then
    mkdir -p "$cache"
    tgz="$cache/$asset.tgz"
    curl -fsSL -o "$tgz" "https://github.com/microsoft/onnxruntime/releases/download/v$VERSION/$asset.tgz"
    if command -v sha256sum > /dev/null; then sha256=(sha256sum); else sha256=(shasum -a 256); fi
    echo "$sha  $tgz" | "${sha256[@]}" -c - >&2
    tar -xzf "$tgz" -C "$cache"
    rm "$tgz"
fi
echo "$cache/$asset"
