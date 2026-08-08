#!/usr/bin/env bash
# Regenerate tests/driver_cst_<key>_oracle_test.wado for a real grammar, pinning
# Gale's parse trees against the published ANTLR4 jar (Stage B'). For each input
# in tests/oracle/<key>/cases.* it runs the ANTLR4 oracle and Gale's parser and
# emits the test pinning the ORACLE tree; a case Gale parses differently becomes
# #[TODO]. Java runs only here; the trees are committed, so CI needs none.
#
# Only clean single combined `WS -> skip` grammars qualify; see "Stage B' for
# real-world grammars" in antlr4-compatibility.md for why Rust/TypeScript
# (split lexer/parser) and css3 (whitespace tokens) are out of scope. Their
# lexers are oracle-able on their own — antlr4-oracle.sh --tokens with
# --super / --stub-super — which this tree-pinning driver does not cover.
#
# Usage: scripts/regen-oracle.sh [sqlite|json|all]   (default: all)
# Needs java+javac and a built `wado` (WADO env, default ../target/release/wado).
set -euo pipefail
cd "$(dirname "$0")/.."

export ANTLR4_VERSION="${ANTLR4_VERSION:-4.13.2}"
WADO="${WADO:-../target/release/wado}"

# Strip ANTLR4's <EOF> (Gale omits it) and collapse whitespace — safe only for
# WS-skipping grammars, where no tree token carries a significant space.
norm() { sed 's/ *<EOF> *//g' | tr '\t\r\n' '   ' | tr -s ' ' | sed 's/^ *//; s/ *$//'; }
# Escape for a Wado "..." literal (backslash first, then quote).
esc() { sed 's/\\/\\\\/g; s/"/\\"/g'; }

regen_one() {
  local key="$1" grammar start module outdir cases
  case "$key" in
    sqlite) grammar="SQLite.g4"; start="parse"; module="sqlite"; outdir="./generated/cst_sqlite"; cases="tests/oracle/sqlite/cases.sql" ;;
    json)   grammar="JSON.g4";   start="json";  module="json";   outdir="./generated/cst_json";   cases="tests/oracle/json/cases.json" ;;
    *) echo "regen-oracle: unknown grammar key '$key'" >&2; return 1 ;;
  esac
  local gpath="tests/grammars/$grammar" out="tests/driver_cst_${key}_oracle_test.wado"

  # 1. Gale's current trees for every case (inline, escaped) to decide #[TODO].
  local dump="tests/_oracle_dump.wado"
  {
    echo 'use { Stdout, println } from "core:cli";'
    echo "use $module from \"./grammars/$grammar\""
    echo "    with { generator: { module: \"../src/generator.wado\", output_dir: \"$outdir\" } };"
    echo "fn d(input: String) with Stdout { let r = ${module}::parse(&input); println(\`@@@\${${module}::to_string_tree(&r)}\`); }"
    echo 'export fn run() with Stdout {'
    while IFS= read -r line; do
      [ -z "$line" ] && continue
      printf '    d("%s");\n' "$(printf '%s' "$line" | esc)"
    done < "$cases"
    echo '}'
  } > "$dump"
  mapfile -t GALE < <($WADO run -O2 "$dump" 2>/dev/null | sed -n 's/^@@@//p')
  rm -f "$dump"

  # 2. Emit the test file.
  {
    echo '#![generated(by = "package-gale/scripts/regen-oracle.sh")]'
    echo "// Do not edit by hand — re-run scripts/regen-oracle.sh $key to regenerate."
    echo "// Generated against ANTLR4 ${ANTLR4_VERSION} (the published jar, run as a black box)."
    echo '//'
    echo "// Stage B' for a real grammar: Gale's $grammar parse trees pinned"
    echo "// against ANTLR4's over $cases. The expected tree is ANTLR4's; a case"
    echo '// Gale parses differently is #[TODO] (resolving it = the divergence closed).'
    echo
    echo "use $module from \"./grammars/$grammar\""
    echo '    with {'
    echo "        generator: { module: \"../src/generator.wado\", output_dir: \"$outdir\" },"
    echo '    };'
    echo "use { normalize_tree } from \"./grammars/$grammar\";"
    echo
    echo 'fn assert_tree(input: &String, expected: &String) {'
    echo "    let result = ${module}::parse(input);"
    echo "    let actual = ${module}::to_string_tree(&result);"
    echo '    let norm = normalize_tree(expected);'
    echo '    assert actual == norm, `\ninput:    ${*input}\nexpected: ${norm}\nactual:   ${actual}`;'
    echo '}'
    echo
    local i=0 todo=0
    while IFS= read -r line; do
      [ -z "$line" ] && continue
      local oracle gale name
      oracle="$(printf '%s' "$line" | bash scripts/antlr4-oracle.sh "$gpath" "$start" 2>/dev/null | norm)"
      gale="$(printf '%s' "${GALE[$i]:-}" | norm)"
      i=$((i + 1))
      name="$(printf '%s' "$line" | tr -d '"\\' | tr -s ' ' | cut -c1-60)"
      if [ "$oracle" != "$gale" ]; then
        todo=$((todo + 1))
        echo "#[TODO]"
      fi
      echo "test \"oracle: $name\" {"
      echo "    assert_tree(&\"$(printf '%s' "$line" | esc)\", &\"$(printf '%s' "$oracle" | esc)\");"
      echo "}"
      echo
    done < "$cases"
    echo "// $key: $i cases, $todo #[TODO] (Gale diverges from ANTLR4)." >&2
  } > "$out"
  echo "wrote $out" >&2
}

case "${1:-all}" in
  all) regen_one sqlite; regen_one json ;;
  *) regen_one "$1" ;;
esac
