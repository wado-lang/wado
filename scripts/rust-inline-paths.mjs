// A `crate::` or `super::` path belongs in a `use` item, not inline where the
// item is read (AGENTS.md > General Rules, which also says why clippy is not
// the gate). The corpus predates the rule, so `rust-inline-paths.json` holds
// what each file still carries and `--check` fails only on a file that grows
// past it.
//
// Usage: node scripts/rust-inline-paths.mjs [--check | --update | <file>…]

import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import { rustFiles, stripNonCode } from "./rust-source.mjs";

const BASELINE_PATH = fileURLToPath(new URL("rust-inline-paths.json", import.meta.url));

// A Rust identifier runs over `XID_Continue`, so `αcrate` is one name and not
// a path root. `$crate` is macro hygiene, which no `use` can replace, and
// `$use` is a macro metavariable rather than an import.
const INLINE_PATH = /(?<![\p{XID_Continue}#$])(?:crate|super)::/gu;
const USE_KEYWORD = /(?<![\p{XID_Continue}#$])use(?![\p{XID_Continue}])/gu;

/** The first non-whitespace character at or after `at`, or "" past the end. */
function nextNonSpace(code, at) {
  let i = at;
  while (i < code.length && /\s/.test(code[i])) i++;
  return code[i] ?? "";
}

/** The half-open span of every `use` item, which may name a path freely. */
function useItemSpans(code) {
  const spans = [];
  for (const match of code.matchAll(USE_KEYWORD)) {
    // `impl Trait + use<'a, T>` is a capture list, not an import. Rust allows
    // any whitespace before the `<`, so the scan cannot be given a width.
    if (nextNonSpace(code, match.index + 3) === "<") continue;
    const semicolon = code.indexOf(";", match.index);
    spans.push([match.index, semicolon < 0 ? code.length : semicolon]);
  }
  return spans;
}

/** Every `crate::` / `super::` written outside a `use` item, in source order. */
export function findInlinePaths(source) {
  const code = stripNonCode(source);
  const spans = useItemSpans(code);
  const lineStarts = [0];
  for (let i = 0; i < code.length; i++) {
    if (code[i] === "\n") lineStarts.push(i + 1);
  }
  const hits = [];
  let line = 1;
  for (const match of code.matchAll(INLINE_PATH)) {
    while (lineStarts[line] !== undefined && lineStarts[line] <= match.index) line++;
    if (spans.some(([from, to]) => match.index >= from && match.index < to)) continue;
    hits.push({ line, column: match.index - lineStarts[line - 1] + 1, text: match[0] });
  }
  return hits;
}

/** File path to the number of inline paths it carries, omitting the clean ones. */
function census(files) {
  const counts = {};
  for (const file of files) {
    const found = findInlinePaths(readFileSync(file, "utf8")).length;
    if (found > 0) counts[file] = found;
  }
  return counts;
}

function readBaseline() {
  return JSON.parse(readFileSync(BASELINE_PATH, "utf8"));
}

function writeBaseline(counts) {
  const sorted = Object.fromEntries(Object.entries(counts).sort(([a], [b]) => (a < b ? -1 : 1)));
  writeFileSync(BASELINE_PATH, `${JSON.stringify(sorted, null, 2)}\n`);
}

const total = (counts) => Object.values(counts).reduce((sum, n) => sum + n, 0);

/** Every file carrying more than its baseline allows, as `[file, was, now]`. */
function grownFiles(counts, baseline) {
  return Object.entries(counts)
    .filter(([file, n]) => n > (baseline[file] ?? 0))
    .map(([file, n]) => [file, baseline[file] ?? 0, n]);
}

function reportGrown(grown) {
  console.error("error: inline `crate::` / `super::` paths added; import them with `use`:");
  for (const [file, was, now] of grown) console.error(`  ${file}: ${was} -> ${now}`);
  console.error("");
  console.error("Run `node scripts/rust-inline-paths.mjs <file>` to list them.");
}

function main(argv) {
  const files = rustFiles();
  if (argv.includes("--update")) {
    const counts = census(files);
    const baseline = readBaseline();
    // The baseline only ever ratchets down. Recording a file that grew would
    // license the paths `--check` is there to refuse.
    const grown = grownFiles(counts, baseline);
    if (grown.length > 0) {
      reportGrown(grown);
      return 1;
    }
    writeBaseline(counts);
    console.log(`baseline: ${total(counts)} inline paths in ${Object.keys(counts).length} files`);
    return 0;
  }
  if (argv.includes("--check")) {
    const counts = census(files);
    const baseline = readBaseline();
    const grown = grownFiles(counts, baseline);
    if (grown.length > 0) {
      reportGrown(grown);
      return 1;
    }
    const left = total(counts);
    const shrunk = total(baseline) - left;
    const ratchet = shrunk > 0 ? `, ${shrunk} fewer than the baseline — run --update` : "";
    console.log(`ok: no file gained an inline path (${left} left${ratchet})`);
    return 0;
  }
  const targets = argv.length > 0 ? argv : files;
  let found = 0;
  for (const file of targets) {
    for (const hit of findInlinePaths(readFileSync(file, "utf8"))) {
      console.log(`${file}:${hit.line}:${hit.column}: ${hit.text}`);
      found++;
    }
  }
  console.log(`${found} inline paths`);
  return 0;
}

if (import.meta.main) process.exitCode = main(process.argv.slice(2));
