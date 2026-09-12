#!/usr/bin/env bash
# Hold every tracked Rust file to the rule that a `crate::` / `super::` path
# belongs in a `use` item (AGENTS.md > General Rules).
#
# Only the driver lives here: the detector is Wado, parses with the
# Gale-generated Rust grammar and needs `wado run`
# (`package-gale/tools/rust_inline_paths.wado`). The baseline it ratchets is
# `scripts/rust-inline-paths.json`.
#
#   scripts/check-rust-paths.sh --check     # fail on a file that grew
#   scripts/check-rust-paths.sh --update    # record the corpus, ratcheting down
#   scripts/check-rust-paths.sh <file.rs>…  # list what those files carry
set -e -o pipefail

cd "$(dirname "$0")/.."

# `wado run` reaches only the current directory, so the corpus list has to
# live inside the repository, not in /tmp.
out=package-gale/build/rust-inline-paths
mkdir -p "${out}"

# Naming files asks about those files; naming none asks about the corpus.
corpus=(--paths-from "${out}/corpus.txt")
for arg in "$@"; do
    case "${arg}" in
    *.rs) corpus=() ;;
    esac
done
if [ "${#corpus[@]}" -gt 0 ]; then
    git ls-files '*.rs' > "${out}/corpus.txt"
fi

cargo build --bin wado
exec ./target/debug/wado run -O3 package-gale/tools/rust_inline_paths.wado -- \
    "${corpus[@]}" "$@"
