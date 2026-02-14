#!/usr/bin/env bash
set -euo pipefail

# Sync vendor/wasmtime submodule to match Cargo.lock wasmtime version.
#
# Usage: mise run sync-wasmtime

# Extract exact wasmtime version from Cargo.lock
VERSION=$(grep -A1 '^name = "wasmtime"$' Cargo.lock | grep '^version' | head -1 | sed 's/version = "//;s/"//')

if [ -z "$VERSION" ]; then
    echo "ERROR: Could not find wasmtime version in Cargo.lock"
    exit 1
fi

TAG="v${VERSION}"
MAJOR="${VERSION%%.*}"
echo "Cargo.lock wasmtime version: ${TAG}"

# Check current submodule state
CURRENT=$(git submodule status vendor/wasmtime 2>/dev/null | awk '{print $1}' | tr -d '+-')
echo "Current vendor/wasmtime commit: ${CURRENT:-(not initialized)}"

# Initialize submodule if needed
if [ ! -d vendor/wasmtime/.git ] && [ ! -f vendor/wasmtime/.git ]; then
    echo "Initializing vendor/wasmtime submodule..."
    git submodule init vendor/wasmtime
    git submodule update vendor/wasmtime
fi

# Fetch the matching tag
echo "Fetching tag ${TAG}..."
if ! git -C vendor/wasmtime fetch origin tag "${TAG}" --no-tags 2>/dev/null; then
    echo "ERROR: Tag ${TAG} not found in wasmtime repository"
    echo "Available v${MAJOR}.* tags:"
    git ls-remote --tags https://github.com/bytecodealliance/wasmtime.git "v${MAJOR}.*" 2>/dev/null \
        | awk '{print "  " $2}' | sed 's|refs/tags/||'
    exit 1
fi

# Checkout the tag
TAG_HASH=$(git -C vendor/wasmtime rev-parse "${TAG}^{}" 2>/dev/null)
echo "Tag ${TAG} -> ${TAG_HASH}"

if [ "${CURRENT}" = "${TAG_HASH}" ]; then
    echo "vendor/wasmtime is already at ${TAG}"
else
    git -C vendor/wasmtime checkout "${TAG}"
    echo "vendor/wasmtime updated to ${TAG}"
fi

# Verify mise.toml wasmtime version consistency
MISE_VERSION=$(grep '^wasmtime' .mise.toml 2>/dev/null | sed 's/.*= "//;s/"//')
if [ -n "${MISE_VERSION}" ] && [ "${MISE_VERSION}" != "${MAJOR}" ]; then
    echo ""
    echo "WARNING: .mise.toml wasmtime = \"${MISE_VERSION}\" does not match major version ${MAJOR}"
    echo "Consider updating .mise.toml: wasmtime = \"${MAJOR}\""
fi

echo ""
echo "Done. Run 'make update-stdlib-wasi' to regenerate WASI stdlib files if needed."
