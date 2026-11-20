#!/usr/bin/env bash
set -euo pipefail

# Fetch the pinned compiler of Google's TPU runtime, which compiles for a
# topology on a machine with no TPU, and print the path of its library. XLA's
# CPU plugin needs no script: `wado-run-pjrt` fetches it when no plugin is
# named. A plugin is a program the tests run, not a dependency anything links,
# so it stays out of the repository and of Cargo. It is cached outside
# `target/`, which CI hands between jobs and every worktree has its own of.
#
# Usage: WADO_PJRT_TPU_PLUGIN=$(scripts/fetch-pjrt-plugin.sh)

ARCHIVE=libtpu-0.0.51-cp312-cp312-manylinux_2_31_x86_64.whl
SHA256=aeb662a3174a89fd9a94ac64c965a72f43c65436f5546abbcb8968c9d9552631
URL="https://files.pythonhosted.org/packages/47/15/c9bb6cbff8447574da7dd5ff4f0ab079d17e49f87059e10e162ebca3ac26/${ARCHIVE}"
MEMBER=libtpu/libtpu.so

if [ "$(uname -sm)" != "Linux x86_64" ]; then
    echo "ERROR: only ${ARCHIVE} is pinned, which does not run on $(uname -sm)" >&2
    exit 1
fi

dir="${XDG_CACHE_HOME:-${HOME}/.cache}/wado/pjrt/${SHA256}"
library="${dir}/$(basename "${MEMBER}")"

if [ ! -f "${library}" ]; then
    mkdir -p "${dir}"
    # Concurrent sessions share the cache, so the library appears whole or not
    # at all.
    fetched=$(mktemp -d "${dir}/fetch.XXXXXX")
    trap 'rm -rf "${fetched}"' EXIT
    curl --fail --silent --show-error --location --retry 3 --output "${fetched}/${ARCHIVE}" "${URL}"
    echo "${SHA256}  ${fetched}/${ARCHIVE}" | sha256sum --check --quiet >&2
    unzip -q "${fetched}/${ARCHIVE}" "${MEMBER}" -d "${fetched}"
    mv "${fetched}/${MEMBER}" "${library}"
fi

echo "${library}"
