#!/usr/bin/env bash
# Compare the parse tree Gale's TypeScript parser builds for one file with the
# ANTLR4 jar's, and list where they disagree. See "Comparing a whole TypeScript
# file" in antlr4-compatibility.md.
#
# Usage: scripts/oracle-typescript.sh [file.ts]
#   The path is relative to the repository root, and defaults to the Gale
#   TypeScript benchmark input. Both trees are kept under
#   package-gale/target/oracle-typescript/.
set -euo pipefail
cd "$(dirname "$0")/../.."

input="${1:-package-gale/benchmark/typescript/input.ts}"
out=package-gale/target/oracle-typescript
mkdir -p "$out"

grammars=package-gale/tests/grammars
rc=0
package-gale/scripts/antlr4-oracle.sh --lexer "$grammars/TypeScriptLexer.g4" \
    --super "$grammars/java/TypeScriptLexerBase.java" \
    --super "$grammars/java/TypeScriptParserBase.java" \
    "$grammars/TypeScriptParser.g4" program < "$input" > "$out/jar.tree" || rc=$?
if [ "$rc" -ne 0 ]; then
    echo "oracle-typescript: the jar did not parse $input (exit $rc)" >&2
    exit 1
fi

scripts/dev-bin.sh wado run package-gale/tools/typescript_tree.wado -- "$input" > "$out/gale.tree"
scripts/dev-bin.sh wado run package-gale/tools/tree_diff.wado -- "$out/jar.tree" "$out/gale.tree"
