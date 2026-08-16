#!/usr/bin/env bash
# Hold `package-gale-highlight-wado/grammar/Wado.g4` to the compiler's own
# parser over the stdlib + fixture corpus.
#
# The two halves live in different runtimes — the compiler's parser is Rust and
# runs in-process, the Gale-generated one is Wasm and needs `wado run` — so the
# driver sits outside both. Everything else (walking the corpus, joining the two
# verdict lists, deciding what counts as a disagreement) is in
# `wado-dev-tools grammar-corpus`.
set -e -o pipefail

cd "$(dirname "$0")/.."

# `wado run` reaches only the current directory, so the corpus list the
# Gale-side tool reads has to live inside the repository, not in /tmp.
out=package-gale-highlight-wado/build/check-grammar
rm -rf "${out}"
mkdir -p "${out}"

cargo build --bin wado --bin wado-dev-tools

./target/debug/wado-dev-tools grammar-corpus --emit-corpus "${out}/corpus.txt"
./target/debug/wado run package-gale-highlight-wado/tools/corpus_check.wado -- \
    --paths-from "${out}/corpus.txt" > "${out}/gale.tsv" 2> "${out}/gale.err" \
    || { cat "${out}/gale.err" >&2; exit 1; }
./target/debug/wado-dev-tools grammar-corpus \
    --compare "${out}/gale.tsv" --report "${out}/divergences.tsv"
