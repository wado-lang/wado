#!/usr/bin/env bash
# Record a CPU profile with `perf` where samply cannot run
# (perf_event_paranoid stays at 2), and convert it to the samply profile
# `analyze_native_profile.ts` reads.
#
#   record_perf.sh OUT.json -- COMMAND [ARGS...]
#
# Leaves OUT.json and, beside it, the raw OUT.perf.data.
set -euo pipefail

if [[ $# -lt 3 || $2 != "--" ]]; then
  echo "usage: record_perf.sh OUT.json -- COMMAND [ARGS...]" >&2
  exit 2
fi
out=$1
shift 2

# Ubuntu's /usr/bin/perf is a wrapper that refuses a kernel it has no
# linux-tools package for, so call the versioned binary directly.
perf=$(ls /usr/lib/linux-tools-*/perf 2>/dev/null | head -1 || true)
if [[ -z $perf ]]; then
  perf=$(command -v perf || true)
fi
if [[ -z $perf ]]; then
  echo "perf not found: sudo apt-get install -y linux-tools-generic" >&2
  exit 1
fi
command -v samply >/dev/null || {
  echo "samply not found: cargo install samply --locked" >&2
  exit 1
}

data=${out%.json}.perf.data
mkdir -p "$(dirname "$out")"
# `:u` keeps the event in user space, which paranoid level 2 allows. The
# compiler is built without frame pointers, so unwind with DWARF.
"$perf" record -e cpu-clock:u -F 499 --call-graph dwarf,16384 -o "$data" -- "$@"
samply import "$data" --save-only -o "$out"
echo "profile: $out" >&2
