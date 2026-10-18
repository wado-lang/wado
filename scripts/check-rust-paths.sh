#!/usr/bin/env bash
# Hold every tracked Rust file to the rule that a `crate::` / `super::` path
# belongs in a `use` item at the top of the module (AGENTS.md > General Rules),
# not in one inside a function body. Only the driver lives
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

exec scripts/dev-bin.sh wado run package-gale/tools/rust_inline_paths.wado -- \
    "${corpus[@]}" "$@"
