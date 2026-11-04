#!/usr/bin/env bash
# Downloads Microsoft's onnxruntime release for this host, unmodified, and
# prints the directory it unpacked to. The version is the one onnxruntime-node
# pins in package-loam/package.json, which Loam's oracles run. Cached under
# ~/.cache/loam.
set -euo pipefail

VERSION=1.31.1
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)
        asset=onnxruntime-linux-x64-$VERSION
        sha=c849e36a1ef3e22d4ff217b9d1ce4a3cf5b9467b975610913ae959ad5b16bdc9
        ;;
    Linux-aarch64)
        asset=onnxruntime-linux-aarch64-$VERSION
        sha=5317505e2da99be65f8e2775ba3c2afd6d2961fd78aba1bd8f56159f48820c78
        ;;
    Darwin-arm64)
        asset=onnxruntime-osx-arm64-$VERSION
        sha=2e1344a1926ee50eb25b38153cb6992c3567515aef0e677a1a520a1798896d6f
        ;;
    *)
        echo "no onnxruntime $VERSION release for $(uname -s)-$(uname -m)" >&2
        exit 1
        ;;
esac

# Unpacked beside the cache and renamed into it, so an interrupted run leaves
# nothing a later one would take for the release.
cache="$HOME/.cache/loam"
if [ ! -d "$cache/$asset" ]; then
    mkdir -p "$cache"
    part="$cache/$asset.part"
    rm -rf "$part"
    mkdir "$part"
    curl -fsSL -o "$part/release.tgz" "https://github.com/microsoft/onnxruntime/releases/download/v$VERSION/$asset.tgz"
    if command -v sha256sum > /dev/null; then sha256=(sha256sum); else sha256=(shasum -a 256); fi
    echo "$sha  $part/release.tgz" | "${sha256[@]}" -c - >&2
    tar -xzf "$part/release.tgz" -C "$part"
    mv "$part/$asset" "$cache/$asset"
    rm -rf "$part"
fi
echo "$cache/$asset"
