#!/usr/bin/env bash
# Compare `package-gale-highlight-wado/grammar/Wado.g4` (through Gale) with the
# compiler's own parser over the stdlib + fixture corpus, and check the result
# against the committed divergence list.
#
#   scripts/check-grammar.sh            verify; fails when the list moved
#   scripts/check-grammar.sh --update   rewrite the list
#
# The compiler side is `wado-dev-tools grammar-corpus`, which asks
# `wado_lsp::Engine::parse_diagnostics` — syntax only, no loader — so a fixture
# whose imports or types are deliberately broken still reports its syntax
# truthfully.
set -e -o pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "${root}"

golden=package-gale-highlight-wado/grammar/divergences.tsv
update=false
if [ "${1:-}" = "--update" ]; then
    update=true
fi

# `wado run` reaches only the current directory, so the corpus list the
# Gale-side tool reads has to live inside the repository, not in /tmp.
tmp=package-gale-highlight-wado/build/check-grammar
rm -rf "${tmp}"
mkdir -p "${tmp}"
trap 'rm -rf "${tmp}"' EXIT

cargo build --bin wado --bin wado-dev-tools

# Corpus: the stdlib and its tests, the e2e fixtures, the format fixtures, and
# the examples. `tests/generated` is excluded — its `*.wir.wado` files are IR
# dumps, not Wado source.
find wado-compiler/lib \
     wado-compiler/tests/fixtures \
     wado-compiler/tests/format.fixtures \
     example \
     -name '*.wado' | sort > "${tmp}/corpus.txt"

./target/debug/wado-dev-tools grammar-corpus --paths-from "${tmp}/corpus.txt" \
    > "${tmp}/compiler.tsv"
./target/debug/wado run package-gale-highlight-wado/tools/corpus_check.wado -- \
    --paths-from "${tmp}/corpus.txt" > "${tmp}/gale.tsv" 2> "${tmp}/gale.err" \
    || { cat "${tmp}/gale.err" >&2; exit 1; }

{
    cat <<'HEADER'
# Files the two Wado parsers disagree about, as of the last
# `scripts/check-grammar.sh --update`. Regenerated, not hand-edited.
#
#   gale-rejects   Wado.g4 reports a syntax error where the compiler parses
#                  cleanly — a grammar gap. Every entry here is work to do.
#   gale-accepts   the compiler's parser rejects where Wado.g4 is happy. Rules
#                  no context-free grammar can state (a chained `!=`, a
#                  non-trailing default argument, `#[serde]` spelled out in a
#                  diagnostic), so they stay.
#
# kind<TAB>path<TAB>line<TAB>message
HEADER
    awk -F'\t' '
        NR == FNR { verdict[$2] = $1; line[$2] = $4; message[$2] = $5; next }
        $1 == verdict[$2] { next }
        $1 == "ng" { printf "gale-rejects\t%s\t%s\t%s\n", $2, $4, $5; next }
        { printf "gale-accepts\t%s\t%s\t%s\n", $2, line[$2], message[$2] }
    ' "${tmp}/compiler.tsv" "${tmp}/gale.tsv" | sort
} > "${tmp}/divergences.tsv"

gaps=$(grep -c '^gale-rejects' "${tmp}/divergences.tsv" || true)
extra=$(grep -c '^gale-accepts' "${tmp}/divergences.tsv" || true)
echo "corpus: $(wc -l < "${tmp}/corpus.txt") files | gale-rejects: ${gaps} | gale-accepts: ${extra}"

if [ "${update}" = true ]; then
    cp "${tmp}/divergences.tsv" "${golden}"
    echo "updated ${golden}"
    exit 0
fi

if ! diff -u "${golden}" "${tmp}/divergences.tsv"; then
    echo "" >&2
    echo "error: the Wado.g4 divergence list moved." >&2
    echo "       Fix the grammar, or re-run with --update if the change is intended." >&2
    exit 1
fi
