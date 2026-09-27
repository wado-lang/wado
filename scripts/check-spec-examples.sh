#!/usr/bin/env bash
# Hold every `wado` block in the specification to quoting an e2e fixture or a
# source file (docs/wep-2026-09-26-spec-examples.md). Only the driver lives
# here: the checker is `package-gale-highlight-wado/tools/spec_examples.wado`,
# which reads the Markdown with Marl and lexes Wado with the `Wado.g4` grammar.
#
#   scripts/check-spec-examples.sh             # check every spec file
#   scripts/check-spec-examples.sh <file.md>…  # check those files
set -e -o pipefail

cd "$(dirname "$0")/.."

# `wado run` reaches only the current directory, so the list has to live inside
# the repository, not in /tmp.
list=package-gale-highlight-wado/build/spec-examples/corpus.txt

# Naming files asks about those files; naming none asks about the corpus.
corpus=(--paths-from "${list}")
if [ "$#" -gt 0 ]; then
    corpus=()
else
    mkdir -p "$(dirname "${list}")"
    git ls-files 'docs/spec-*.md' > "${list}"
fi

cargo build --bin wado
exec ./target/debug/wado run package-gale-highlight-wado/tools/spec_examples.wado -- \
    "${corpus[@]}" "$@"
