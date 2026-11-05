#!/usr/bin/env bash
set -euo pipefail

# Fetch the pinned build of XLA's CPU plugin and print the path of its library.
# A plugin is a program the tests run, not a dependency anything links, so it
# stays out of the repository and of Cargo. It is cached outside `target/`, which
# CI hands between jobs and every worktree has its own of.
#
# Usage: WADO_PJRT_PLUGIN=$(scripts/fetch-pjrt-plugin.sh)

VERSION=v18.0.0
ARCHIVE=pjrt-cpu_linux-amd64.tar.gz
SHA256=6fbaa0cd1ff4d84fa7000ce5acc00e817bdf74c85cef1cc42c7cccc376c56431

if [ "$(uname -sm)" != "Linux x86_64" ]; then
    echo "ERROR: only ${ARCHIVE} is pinned, which does not run on $(uname -sm)" >&2
    exit 1
fi

dir="${XDG_CACHE_HOME:-${HOME}/.cache}/wado/pjrt/${SHA256}"
library="${dir}/libpjrt_cpu.so"

if [ ! -f "${library}" ]; then
    mkdir -p "${dir}"
    # Concurrent sessions share the cache, so the library appears whole or not
    # at all.
    fetched=$(mktemp -d "${dir}/fetch.XXXXXX")
    trap 'rm -rf "${fetched}"' EXIT
    curl --fail --silent --show-error --location --retry 3 --output "${fetched}/${ARCHIVE}" \
        "https://github.com/zml/pjrt-artifacts/releases/download/${VERSION}/${ARCHIVE}"
    echo "${SHA256}  ${fetched}/${ARCHIVE}" | sha256sum --check --quiet >&2
    tar --extract --gzip --file "${fetched}/${ARCHIVE}" --directory "${fetched}"
    mv "${fetched}/libpjrt_cpu.so" "${library}"
fi

echo "${library}"
