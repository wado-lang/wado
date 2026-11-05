#!/usr/bin/env bash
set -euo pipefail

# Fetch the pinned build of XLA's CPU plugin and print the path of its library.
# A plugin is a program the tests run, not a dependency anything links, so it
# stays out of the repository and of Cargo.
#
# Usage: WADO_PJRT_PLUGIN=$(scripts/fetch-pjrt-plugin.sh)

VERSION=v18.0.0
ARCHIVE=pjrt-cpu_linux-amd64.tar.gz
SHA256=6fbaa0cd1ff4d84fa7000ce5acc00e817bdf74c85cef1cc42c7cccc376c56431

if [ "$(uname -sm)" != "Linux x86_64" ]; then
    echo "ERROR: only ${ARCHIVE} is pinned, which does not run on $(uname -sm)" >&2
    exit 1
fi

root="$(cd "$(dirname "$0")/.." && pwd)"
dir="${CARGO_TARGET_DIR:-${root}/target}/pjrt/${SHA256}"
library="${dir}/libpjrt_cpu.so"

if [ ! -f "${library}" ]; then
    mkdir -p "${dir}"
    curl --fail --silent --show-error --location --retry 3 --output "${dir}/${ARCHIVE}" \
        "https://github.com/zml/pjrt-artifacts/releases/download/${VERSION}/${ARCHIVE}"
    echo "${SHA256}  ${dir}/${ARCHIVE}" | sha256sum --check --quiet >&2
    tar --extract --gzip --file "${dir}/${ARCHIVE}" --directory "${dir}"
    rm "${dir}/${ARCHIVE}"
fi

echo "${library}"
