#!/usr/bin/env bash
set -euo pipefail

# Sync all vendor submodules.
#
# - vendor/wasmtime: pinned to the exact version in Cargo.lock
# - vendor/wasm, vendor/wasi, vendor/wasm-tools, vendor/component-model: updated to latest remote HEAD
#
# Usage: mise run sync-vendor

# --- vendor/wasmtime: sync to Cargo.lock version ---

echo "==> Syncing vendor/wasmtime to Cargo.lock version"

VERSION=$(grep -A1 '^name = "wasmtime"$' Cargo.lock | grep '^version' | head -1 | sed 's/version = "//;s/"//')

if [ -z "$VERSION" ]; then
    echo "ERROR: Could not find wasmtime version in Cargo.lock"
    exit 1
fi

TAG="v${VERSION}"
MAJOR="${VERSION%%.*}"
echo "Cargo.lock wasmtime version: ${TAG}"

# Initialize submodule if needed
if [ ! -d vendor/wasmtime/.git ] && [ ! -f vendor/wasmtime/.git ]; then
    echo "Initializing vendor/wasmtime submodule..."
    git submodule init vendor/wasmtime
    git submodule update vendor/wasmtime
fi

CURRENT=$(git -C vendor/wasmtime rev-parse HEAD 2>/dev/null)

# Fetch the matching tag
echo "Fetching tag ${TAG}..."
if ! git -C vendor/wasmtime fetch origin tag "${TAG}" --no-tags 2>/dev/null; then
    echo "ERROR: Tag ${TAG} not found in wasmtime repository"
    echo "Available v${MAJOR}.* tags:"
    git ls-remote --tags https://github.com/bytecodealliance/wasmtime.git "v${MAJOR}.*" 2>/dev/null \
        | awk '{print "  " $2}' | sed 's|refs/tags/||'
    exit 1
fi

TAG_HASH=$(git -C vendor/wasmtime rev-parse "${TAG}^{}" 2>/dev/null)

if [ "${CURRENT}" = "${TAG_HASH}" ]; then
    echo "vendor/wasmtime is already at ${TAG}"
else
    git -C vendor/wasmtime checkout "${TAG}"
    echo "vendor/wasmtime updated to ${TAG}"
fi

# Auto-sync mise.toml wasmtime CLI version
MISE_VERSION=$(grep '^wasmtime' mise.toml 2>/dev/null | sed 's/.*= "//;s/"//')
if [ -n "${MISE_VERSION}" ] && [ "${MISE_VERSION}" != "${MAJOR}" ]; then
    echo "Updating mise.toml: wasmtime = \"${MISE_VERSION}\" -> \"${MAJOR}\""
    sed -i "s/^wasmtime = \"${MISE_VERSION}\"/wasmtime = \"${MAJOR}\"/" mise.toml
    mise lock wasmtime 2>/dev/null || true
fi

# --- Other vendors: update to latest ---

for submodule in vendor/wasm vendor/wasi vendor/wasm-tools vendor/component-model vendor/jco vendor/antlr4; do
    echo ""
    echo "==> Updating ${submodule} to latest"
    git submodule update --init --remote "${submodule}"
done

echo ""
echo "Done. Run 'mise run update-stdlib-wasi' to regenerate WASI stdlib files if needed."
