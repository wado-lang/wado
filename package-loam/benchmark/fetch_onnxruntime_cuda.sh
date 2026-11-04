#!/usr/bin/env bash
# Downloads Microsoft's onnxruntime release built for CUDA 13, unmodified, and
# the CUDA libraries its CUDA provider loads, from NVIDIA's redistributables.
# Prints the directory they unpacked to, its `lib` holding them all, or nothing
# on a host with no NVIDIA driver or no release. The version is the one
# onnxruntime-node pins in package-loam/package-lock.json, which Loam's
# oracles run; CUDA's is the newest the driver this was measured on runs.
# Cached under ~/.cache/loam.
set -euo pipefail

VERSION=1.31.1
CUDA=13.2.2
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)
        asset=onnxruntime-linux-x64-gpu_cuda13-$VERSION
        sha=b2146862e691f7e338f5aff9c7a9a5567c2ca57cf01442a1adb3a13bf62d9095
        ;;
    *)
        exit 0
        ;;
esac
if [ ! -e /usr/lib/x86_64-linux-gnu/libcuda.so.1 ] && ! ldconfig -p 2> /dev/null | grep -q libcuda.so.1; then
    exit 0
fi

# Each library's archive in NVIDIA's redistributables, as redistrib_$CUDA.json
# lists it, with its SHA-256.
redist=https://developer.download.nvidia.com/compute/cuda/redist
libraries=(
    "cuda_cudart/linux-x86_64/cuda_cudart-linux-x86_64-13.2.86-archive.tar.xz d6be2b3546fa93e5dec9e3d118d31f9540d0b7af5319ee3c6726f202dfc5b5d4"
    "libcublas/linux-x86_64/libcublas-linux-x86_64-13.4.1.3-archive.tar.xz 2ae4563b6f49f29c65cd550146a7acc9b12633e11c2c9d748545ca7fc5cfcd74"
    "libcurand/linux-x86_64/libcurand-linux-x86_64-10.4.2.66-archive.tar.xz f682166d50b81d3f1833c1d5a29cfe06871ec8e48991ffdeac367ba6430cf6a7"
    "cuda_nvrtc/linux-x86_64/cuda_nvrtc-linux-x86_64-13.2.86-archive.tar.xz 6e0661e31a43c22ad030637adeb881822249ffb2a14a19691828f117f5ab2de9"
)

# Assembled beside the cache and renamed into it, so an interrupted run leaves
# nothing a later one would take for the release.
if command -v sha256sum > /dev/null; then sha256=(sha256sum); else sha256=(shasum -a 256); fi
dir="$HOME/.cache/loam/$asset-cuda-$CUDA"
if [ ! -d "$dir" ]; then
    part="$dir.part"
    rm -rf "$part"
    mkdir -p "$part/archive"
    curl -fsSL -o "$part/archive/release.tgz" "https://github.com/microsoft/onnxruntime/releases/download/v$VERSION/$asset.tgz"
    echo "$sha  $part/archive/release.tgz" | "${sha256[@]}" -c - >&2
    tar -xzf "$part/archive/release.tgz" -C "$part" --strip-components 1
    for entry in "${libraries[@]}"; do
        read -r path library_sha <<< "$entry"
        curl -fsSL -o "$part/archive/library.tar.xz" "$redist/$path"
        echo "$library_sha  $part/archive/library.tar.xz" | "${sha256[@]}" -c - >&2
        tar -xJf "$part/archive/library.tar.xz" -C "$part/lib" --strip-components 2 --exclude '*/stubs/*' --wildcards '*/lib/*.so*'
    done
    rm -rf "$part/archive"
    mv "$part" "$dir"
fi
echo "$dir"
