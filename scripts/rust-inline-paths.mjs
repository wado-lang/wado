// A `crate::` or `super::` path belongs in a `use` item, not inline where the
// item is read (AGENTS.md > General Rules, which also says why clippy is not
// the gate). The corpus predates the rule, so `rust-inline-paths.json` holds
// what each file still carries and `--check` fails only on a file that grows
// past it.
//
// Usage: node scripts/rust-inline-paths.mjs [--check | --update | <file>…]

import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const BASELINE_PATH = fileURLToPath(new URL("rust-inline-paths.json", import.meta.url));

// `$crate` is macro hygiene, which no `use` can replace.
const INLINE_PATH = /(?<![A-Za-z0-9_#$])(?:crate|super)::/g;
const USE_KEYWORD = /(?<![A-Za-z0-9_#])use(?![A-Za-z0-9_])/g;

/** Index past the char literal at `at`, or -1 when the quote opens a lifetime. */
function charLiteralEnd(source, at) {
  if (source[at + 1] === "\\") {
    let i = at + 3;
    while (i < source.length && source[i] !== "'") i++;
    return i < source.length ? i + 1 : -1;
  }
  return source[at + 2] === "'" ? at + 3 : -1;
}

/** Index past the string literal at `at`, or -1 when none starts there. */
function stringLiteralEnd(source, at) {
  let i = at;
  if (source[i] === "b" || source[i] === "c") i++;
  const raw = source[i] === "r";
  if (raw) i++;
  let hashes = 0;
  while (raw && source[i] === "#") {
    hashes++;
    i++;
  }
  if (source[i] !== '"') return -1;
  if (raw) {
    const close = `"${"#".repeat(hashes)}`;
    const end = source.indexOf(close, i + 1);
    return end < 0 ? source.length : end + close.length;
  }
  i++;
  while (i < source.length) {
    if (source[i] === "\\") i += 2;
    else if (source[i] === '"') return i + 1;
    else i++;
  }
  return source.length;
}

/**
 * The source with comments and literals blanked out, every other byte and every
 * newline in place, so an offset into the result is an offset into the source.
 */
export function stripNonCode(source) {
  const out = source.split("");
  const blank = (from, to) => {
    for (let i = from; i < to; i++) {
      if (out[i] !== "\n") out[i] = " ";
    }
  };
  let i = 0;
  while (i < source.length) {
    const pair = source.slice(i, i + 2);
    if (pair === "//") {
      const newline = source.indexOf("\n", i);
      const end = newline < 0 ? source.length : newline;
      blank(i, end);
      i = end;
    } else if (pair === "/*") {
      let depth = 1;
      let j = i + 2;
      while (j < source.length && depth > 0) {
        const inner = source.slice(j, j + 2);
        if (inner === "/*") {
          depth++;
          j += 2;
        } else if (inner === "*/") {
          depth--;
          j += 2;
        } else {
          j++;
        }
      }
      blank(i, j);
      i = j;
    } else if (source[i] === "'") {
      const end = charLiteralEnd(source, i);
      if (end < 0) {
        i++;
      } else {
        blank(i, end);
        i = end;
      }
    } else {
      const end = stringLiteralEnd(source, i);
      if (end < 0) {
        i++;
      } else {
        blank(i, end);
        i = end;
      }
    }
  }
  return out.join("");
}

/** The half-open span of every `use` item, which may name a path freely. */
function useItemSpans(code) {
  const spans = [];
  for (const match of code.matchAll(USE_KEYWORD)) {
    // `impl Trait + use<'a, T>` is a capture list, not an import.
    if (/^\s*</.test(code.slice(match.index + 3, match.index + 8))) continue;
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

/** Every tracked Rust file, which is the corpus the rule covers. */
function rustFiles() {
  const listed = execFileSync("git", ["ls-files", "-z", "*.rs"], { encoding: "utf8" });
  return listed.split("\0").filter(Boolean);
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

function main(argv) {
  const files = rustFiles();
  if (argv.includes("--update")) {
    const counts = census(files);
    writeBaseline(counts);
    console.log(`baseline: ${total(counts)} inline paths in ${Object.keys(counts).length} files`);
    return 0;
  }
  if (argv.includes("--check")) {
    const counts = census(files);
    const baseline = readBaseline();
    const grown = Object.entries(counts).filter(([file, n]) => n > (baseline[file] ?? 0));
    if (grown.length > 0) {
      console.error("error: inline `crate::` / `super::` paths added; import them with `use`:");
      for (const [file, n] of grown) console.error(`  ${file}: ${baseline[file] ?? 0} -> ${n}`);
      console.error("");
      console.error("Run `node scripts/rust-inline-paths.mjs <file>` to list them.");
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
