#!/usr/bin/env bash
# Hold every tracked Rust file to the rule that a `crate::` / `super::` path
# belongs in a `use` item (AGENTS.md > Writing). Only the driver lives
# here: the detector is `package-gale/tools/rust_inline_paths.wado`, which
# parses with the Gale Rust grammar and needs `wado run`.
#
#   scripts/check-rust-paths.sh --check     # fail on a file that gained one
#   scripts/check-rust-paths.sh --update    # record the corpus, ratcheting down
#   scripts/check-rust-paths.sh <file.rs>…  # list what those files carry
set -e -o pipefail

cd "$(dirname "$0")/.."

# `wado run` reaches only the current directory, so the list has to live inside
# the repository, not in /tmp.
list=package-gale/build/rust-inline-paths/corpus.txt

# Naming files asks about those files; naming none asks about the corpus.
corpus=(--paths-from "${list}")
for arg in "$@"; do
    case "${arg}" in
    *.rs) corpus=() ;;
    esac
done
if [ "${#corpus[@]}" -gt 0 ]; then
    mkdir -p "$(dirname "${list}")"
    git ls-files '*.rs' > "${list}"
fi

# The tool is compiled on every run, and `-O2` takes longer to compile than it
# saves in running; `-O1` is the fastest end to end.
exec scripts/dev-bin.sh wado run -O1 package-gale/tools/rust_inline_paths.wado -- \
    "${corpus[@]}" "$@"
