// Every name the compiler mints for itself starts with one `$`
// (`name::INTERNAL_PREFIX`), which no Wado identifier can spell, so such a name
// collides with nothing an author wrote. A `__` name is the old convention, and
// `--check` refuses a new one. That includes a name appended to another
// (`format!("{base}__n_{field}")`), which carries only the prefix `base` has.
//
// Usage: node scripts/internal-names.mjs [--check | <file>…]

import { readFileSync } from "node:fs";

import { rustFiles, stringLiterals } from "./rust-source.mjs";

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
 * Every literal in `source` that mints a `__…` name, as `{ line, text }`. A
 * literal mints one at its start, and after a `{…}` interpolation, where the
 * name is appended to another and inherits that one's prefix. The `}` stays in
 * the token, so the two positions are allowed apart. Further along is prose or a
 * mangle separator.
 */
export function findLegacyNames(source) {
  const hits = [];
  for (const { line, text, content } of stringLiterals(source)) {
    const names = [...content.matchAll(/(?:^|\})__[A-Za-z0-9_]*/g)];
    if (names.some((name) => !ALLOWED.has(name[0]))) {
      hits.push({ line, text });
    }
  }
  return hits;
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
