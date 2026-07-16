#!/usr/bin/env bash
# Regenerate tests/driver_cst_sqlite_oracle_test.wado: pin Gale's SQLite parse
# trees against the published ANTLR4 jar (Stage B', extended to a real grammar).
#
# For each input in tests/oracle/sqlite/cases.sql this runs the ANTLR4 oracle
# (scripts/antlr4-oracle.sh, the published jar as a black box) to get the
# reference tree, and Gale's generated parser to get its current tree. The
# committed test pins the ORACLE tree; a case where Gale currently diverges is
# emitted #[TODO], so resolving the TODO is the signal that the divergence
# closed. Inputs omit the trailing ";" so the sql_stmt_list trailing-separator
# shape stays out of scope.
#
# Needs java+javac (oracle) and a built `wado` (WADO env var, default
# release binary). Not run in CI — the trees are committed.
set -euo pipefail
cd "$(dirname "$0")/.."

GRAMMAR="tests/grammars/SQLite.g4"
CASES="tests/oracle/sqlite/cases.sql"
OUT="tests/driver_cst_sqlite_oracle_test.wado"
export ANTLR4_VERSION="${ANTLR4_VERSION:-4.13.2}"
WADO="${WADO:-../target/release/wado}"

# Strip ANTLR4's <EOF> marker (cosmetic; Gale omits it) and collapse whitespace.
norm() { sed 's/ *<EOF> *//g' | tr '\t\r\n' '   ' | tr -s ' ' | sed 's/^ *//; s/ *$//'; }

# --- 1. Gale's current trees for every case, to decide #[TODO]. -------------
DUMP="tests/_oracle_dump.wado"
{
  echo 'use { Stdout, println } from "core:cli";'
  echo 'use sqlite from "./grammars/SQLite.g4"'
  echo '    with { generator: { module: "../src/generator.wado", output_dir: "./generated/cst_sqlite" } };'
  echo 'fn d(input: String) with Stdout { let r = sqlite::parse(&input); println(`@@@{sqlite::to_string_tree(&r)}`); }'
  echo 'export fn run() with Stdout {'
  while IFS= read -r line; do
    [ -z "$line" ] && continue
    echo "    d(\"$line\");"
  done < "$CASES"
  echo '}'
} > "$DUMP"
mapfile -t GALE < <($WADO run -O2 "$DUMP" 2>/dev/null | sed -n 's/^@@@//p')
rm -f "$DUMP"

# --- 2. Emit the test file. -------------------------------------------------
{
  echo '#![generated(by = "package-gale/scripts/regen-sqlite-oracle.sh")]'
  echo '// Do not edit by hand — re-run scripts/regen-sqlite-oracle.sh to regenerate.'
  echo "// Generated against ANTLR4 ${ANTLR4_VERSION} (the published jar, run as a black box)."
  echo '//'
  echo "// Stage B' for a real grammar: Gale's SQLite parse trees pinned against"
  echo "// ANTLR4's over tests/oracle/sqlite/cases.sql. The expected tree is always"
  echo "// ANTLR4's; a case Gale currently parses differently is #[TODO] (resolving"
  echo '// it = the divergence closed). Inputs omit the trailing ";".'
  echo
  echo 'use sqlite from "./grammars/SQLite.g4"'
  echo '    with {'
  echo '        generator: { module: "../src/generator.wado", output_dir: "./generated/cst_sqlite" },'
  echo '    };'
  echo 'use { normalize_tree } from "./grammars/SQLite.g4";'
  echo
  echo 'fn assert_tree(input: &String, expected: &String) {'
  echo '    let result = sqlite::parse(input);'
  echo '    let actual = sqlite::to_string_tree(&result);'
  echo '    let norm = normalize_tree(expected);'
  echo '    assert actual == norm, `\ninput:    {*input}\nexpected: {norm}\nactual:   {actual}`;'
  echo '}'
  echo
  i=0
  todo_count=0
  while IFS= read -r line; do
    [ -z "$line" ] && continue
    oracle="$(printf '%s' "$line" | bash scripts/antlr4-oracle.sh "$GRAMMAR" parse 2>/dev/null | norm)"
    gale="$(printf '%s' "${GALE[$i]:-}" | norm)"
    i=$((i + 1))
    name="$(printf '%s' "$line" | tr -s ' ')"
    if [ "$oracle" != "$gale" ]; then
      todo_count=$((todo_count + 1))
      echo "#[TODO]"
    fi
    echo "test \"oracle: ${name}\" {"
    echo "    assert_tree(&\"${line}\", &\"${oracle}\");"
    echo "}"
    echo
  done < "$CASES"
  echo "// $i cases, $todo_count #[TODO] (Gale diverges from ANTLR4)." >&2
} > "$OUT"
echo "wrote $OUT" >&2
