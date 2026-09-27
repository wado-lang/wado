#!/usr/bin/env bash
# Hold the Gale highlighter (`Wado.g4` + `Wado.highlights.scm`) to the
# compiler's own classification over the stdlib + fixture corpus.
#
# The sibling of `check-grammar.sh`, which compares parse verdicts. Same split:
# the compiler's classifier is Rust and in-process, the Gale-generated one is
# Wasm and needs `wado run`. Everything else is in
# `wado-dev-tools highlight-corpus`.
set -e -o pipefail

cd "$(dirname "$0")/.."

# `wado run` reaches only the current directory, so the corpus list has to
# live inside the repository, not in /tmp.
out=package-gale-highlight-wado/build/check-highlight
rm -rf "${out}"
mkdir -p "${out}"

# The vocabulary check is cheap and explains most class divergences, so fail
# on it first rather than through thousands of corpus rows.
scripts/dev-bin.sh wado-dev-tools highlight-vocab

scripts/dev-bin.sh wado-dev-tools highlight-corpus --emit-corpus "${out}/corpus.txt"
# `-O1`, for the reason `check-rust-paths.sh` gives.
scripts/dev-bin.sh wado run -O1 package-gale-highlight-wado/tools/highlight_dump.wado -- \
    --paths-from "${out}/corpus.txt" > "${out}/gale.tsv" 2> "${out}/gale.err" \
    || { cat "${out}/gale.err" >&2; exit 1; }
scripts/dev-bin.sh wado-dev-tools highlight-corpus \
    --compare "${out}/gale.tsv" --report "${out}/divergences.tsv"
