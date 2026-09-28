#!/usr/bin/env bash
# Coverage is counted from source, so no optimization may change it: a pass
# that moves a probe out of its region, or drops one that could run, shows here
# as a difference. Runs the stdlib tests under coverage at -O0 and -O3 and
# requires the same regions left unrun. Which test ran a region may differ: a
# test that waits on the host or a clock takes a path its timing picks.
set -e -o pipefail

cd "$(dirname "$0")/.."

export MIMALLOC_PURGE_DELAY=-1
out=build/coverage-levels
mkdir -p "${out}"
wado=$(scripts/dev-bin.sh --path wado)
for level in O0 O3; do
    "${wado}" test "-${level}" wado-compiler --coverage=baseline --coverage-include=stdlib \
        --format tap > "${out}/${level}.log"
    cp build/coverage/baseline.json "${out}/${level}.json"
done
if ! diff -u "${out}/O0.json" "${out}/O3.json" > "${out}/diff.txt"; then
    head -200 "${out}/diff.txt"
    echo "error: coverage differs between -O0 and -O3 (full diff: ${out}/diff.txt)" >&2
    exit 1
fi
