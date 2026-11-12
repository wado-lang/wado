// Writes tests/generated/test262_test.wado: each pattern test262 says the
// grammar rejects, as a parse-phase SyntaxError, and that Node's RegExp
// rejects with the `u` flag too, held to both Re and validate_ecma262_syntax
// rejecting it. test262 is read as data: the literal a test evaluates, and the
// metadata that says it must fail. Node only confirms that the `u` grammar is
// the one rejecting it, since a test without the flag may fail for want of
// Annex B instead. A property `core:icu` knows is held to both accepting it
// instead.
//
// Run from the repository root, with vendor/test262 checked out:
// mise run test262-re

import { readFileSync, writeFileSync } from "node:fs";
import { basename, join, relative } from "node:path";
import { acceptsUnderU, corpus, files, root } from "./test262-corpus.mjs";
import { wadoLiteral as wado, wadoPattern } from "./wado-literal.mjs";

const dirs = ["test/language/literals/regexp", "test/built-ins/RegExp/property-escapes"];

// The literal a negative test evaluates: the last line after
// `$DONOTEVALUATE();`, when it is one regular expression literal.
function literal(source) {
  const body = source.split("$DONOTEVALUATE();")[1];
  if (body === undefined) {
    return null;
  }
  const lines = body.split("\n").filter((line) => line.trim() !== "");
  if (lines.length !== 1) {
    return null;
  }
  // Some tests mark the literal with a comment after it: `/(?M:a)//*{ … }*/;`
  // or `/(?\u0069:a)/u /* i */;`.
  const line = lines[0].replace(/\/\*.*\*\/;?\s*$/, "").trim();
  const m = /^\/(.+)\/([dgimsuy]*);?$/.exec(line);
  return m === null ? null : m[1];
}

// The tests naming a property ECMA-262's tables leave out and `core:icu`
// knows. Re takes `\p` from `core:icu`, so it accepts these where test262
// requires a rejection. `\p{Line_Break}` names no value, so it stays an error;
// its negated test names one.
const icuProperties = [
  "unsupported-binary-property-Full_Composition_Exclusion",
  "unsupported-binary-property-Grapheme_Link",
  "unsupported-binary-property-Hyphen",
  "unsupported-binary-property-Prepended_Concatenation_Mark",
  "unsupported-property-Line_Break-with-value",
].flatMap((stem) => [`${stem}.js`, `${stem}-negated.js`]).concat(["unsupported-property-Line_Break-negated.js"]);

function knownToIcu(path) {
  return icuProperties.includes(basename(path));
}


const lines = [
  '#![generated(by = "package-re/scripts/test262.mjs", sources = ["vendor/test262"])]',
  "// The patterns test262's parse-phase negative tests hold to a SyntaxError,",
  "// which Node's RegExp rejects with the `u` flag too. Regenerate with",
  "// `mise run test262-re`; do not edit.",
  "",
  'use { Regex, validate_ecma262_syntax } from "../../src/lib.wado";',
];
for (const dir of dirs) {
  for (const path of files(join(corpus, dir))) {
    const source = readFileSync(path, "utf8");
    if (!/negative:\s*\n\s*phase: parse\s*\n\s*type: SyntaxError/.test(source)) {
      continue;
    }
    const pattern = literal(source);
    if (pattern === null || acceptsUnderU(pattern)) {
      continue;
    }
    const name = relative(corpus, path);
    const known = knownToIcu(name);
    const verdict = known ? "is_ok" : "is_err";
    const p = wadoPattern(pattern);
    if (known) {
      lines.push("", `// core:icu knows the property, so Re accepts it.`);
    }
    lines.push("", `test ${wado(name)} {`, `    assert Regex::new(${p}).${verdict}();`, `    assert validate_ecma262_syntax(${p}).${verdict}();`, "}");
  }
}
writeFileSync(join(root, "tests/generated/test262_test.wado"), `${lines.join("\n")}\n`);
