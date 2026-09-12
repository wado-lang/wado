// Every name the compiler mints for itself starts with one `$`
// (`name::INTERNAL_PREFIX`): the lexer admits none in an identifier, so such a
// name cannot collide with one an author wrote, and a dump says at a glance
// which names are the compiler's. A `__`-prefixed string literal in Rust is the
// old convention, and `--check` refuses a new one — including one appended to
// another name (`format!("{base}__n_{field}")`), which carries only whatever
// prefix `base` happens to have.
//
// Usage: node scripts/internal-names.mjs [--check | <file>…]

import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";

/** Literals that are not names the compiler mints. */
const ALLOWED = new Map([
  ["__DATA__", "the data-section marker, which source spells"],
  ["__", "the legacy prefix itself, testing a source-level field name"],
  ["__cm_packed", "a field lib/core/prelude/types.wado declares"],
  ["__cm_outptr", "a field lib/core/prelude/types.wado declares"],
  ["__cm_size", "a field lib/core/prelude/types.wado declares"],
  ["__cm_align", "a field lib/core/prelude/types.wado declares"],
  ["__cm_lift", "a field lib/core/prelude/types.wado declares"],
  ["__wado_query__", "a synthetic module path, not an identifier"],
  ["__wado_probe__", "a synthetic module path, not an identifier"],
  ["__wasm_import_dce_entry__", "a test's synthetic module path"],
  ["__wasm_import_dce_unused_entry__", "a test's synthetic module path"],
  ["__libm_dce_entry__", "a test's synthetic module path"],
  ["__libm_data_dce_entry__", "a test's synthetic module path"],
  ["________ok", "what `test_name_to_snake` makes of a non-ASCII name"],
  ["______", "what `test_name_to_snake` makes of a non-ASCII name"],
  ["____ABCDEFGHIJKLMNOP", "test data"],
  ["}__", "a label spliced under `$inline_…`, which carries the prefix already"],
]);

/**
 * Every literal in `source` that mints a `__…` name, as `{ line, text }` in
 * source order. Two positions mint one: the literal's own start, and right
 * after a `{…}` interpolation, where a name is appended to another name and
 * inherits whatever prefix that one carries. The `}` is kept in the token so
 * the two positions are allowed separately. Everything further along is prose
 * or a mangle separator, and the name the literal opens with is the same
 * allowance for all of it.
 */
export function findLegacyNames(source) {
  const hits = [];
  let line = 1;
  for (const match of source.matchAll(/\n|"(?:[^"\\\n]|\\.)*"/g)) {
    if (match[0] === "\n") {
      line++;
      continue;
    }
    const names = match[0].slice(1, -1).matchAll(/(?:^|\})__[A-Za-z0-9_]*/g);
    if ([...names].some((name) => !ALLOWED.has(name[0]))) {
      hits.push({ line, text: match[0] });
    }
  }
  return hits;
}

/** Every tracked Rust file, which is the corpus the rule covers. */
function rustFiles() {
  const listed = execFileSync("git", ["ls-files", "-z", "*.rs"], { encoding: "utf8" });
  return listed.split("\0").filter(Boolean);
}

function main(argv) {
  const check = argv.includes("--check");
  const targets = argv.filter((arg) => arg !== "--check");
  const files = targets.length > 0 ? targets : rustFiles();
  let found = 0;
  for (const file of files) {
    for (const hit of findLegacyNames(readFileSync(file, "utf8"))) {
      console.log(`${file}:${hit.line}: ${hit.text}`);
      found++;
    }
  }
  if (!check) {
    console.log(`${found} legacy names`);
    return 0;
  }
  if (found > 0) {
    console.error("");
    console.error(
      "error: a name the compiler mints starts with one `$`" +
        " (wado-compiler/src/name.rs > INTERNAL_PREFIX), not `__`.",
    );
    console.error(
      "A name Wado source must spell belongs in this script's ALLOWED list," +
        " with the reason.",
    );
    return 1;
  }
  console.log("ok: every minted name carries the internal prefix");
  return 0;
}

if (import.meta.main) process.exitCode = main(process.argv.slice(2));
