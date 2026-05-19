// JavaScript syntax highlighter benchmarks for SQL.
// Comparison baselines for the Gale-generated SQLite highlighter.
//
// Highlighters:
//   - Prism.js          (regex-based, the speed reference)
//   - Lezer             (pure-JS LR parser; @codemirror/lang-sql + @lezer/highlight)
//   - tree-sitter (JS)  (web-tree-sitter, the official WASM-via-JS binding,
//                        using the same SQL grammar as the Rust native row)
//   - Shiki (JS engine) (TextMate grammars, VSCode-quality reference)
//
// How to run:
//   node syntax_highlight.js
// (Run `npm install` here first.)

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import Prism from "prismjs";
import "prismjs/components/prism-sql.js";

import { sql as sqlLang, SQLite } from "@codemirror/lang-sql";
import { classHighlighter, highlightTree } from "@lezer/highlight";

import { Language, Parser, Query } from "web-tree-sitter";

import { createHighlighter, createJavaScriptRegexEngine } from "shiki";

const __dirname = dirname(fileURLToPath(import.meta.url));
const SQL_PATH = join(__dirname, "..", "sqlite_parse", "queries.sql");
const sql = readFileSync(SQL_PATH, "utf-8");

const ITERATIONS = 100;

function report(name, elapsedMs) {
  const elapsedUs = elapsedMs * 1000;
  const perIterUs = elapsedUs / ITERATIONS;
  const msInt = Math.floor(elapsedMs);
  const msFrac = Math.floor((elapsedMs - msInt) * 1000);
  const piInt = Math.floor(perIterUs);
  const piFrac = Math.floor((perIterUs - piInt) * 1000);
  console.log(
    `Elapsed: ${msInt}.${String(msFrac).padStart(3, "0")} ms (${ITERATIONS} iterations)`,
  );
  console.log(`Per iteration: ${piInt}.${String(piFrac).padStart(3, "0")} us`);
}

function bench(label, run) {
  console.log(`\n=== ${label} ===`);
  console.log(
    `syntax-highlight (${label}): ${sql.length} bytes, ${ITERATIONS} iterations`,
  );

  const warm = run();
  if (!warm || warm.length === 0) {
    throw new Error(`${label}: warm-up produced empty output`);
  }

  const start = performance.now();
  let lastLen = 0;
  for (let i = 0; i < ITERATIONS; i++) {
    lastLen = run().length;
  }
  const elapsed = performance.now() - start;

  if (lastLen === 0) {
    throw new Error(`${label}: produced empty output`);
  }
  report(label, elapsed);
}

const ESC = { "<": "&lt;", ">": "&gt;", "&": "&amp;", '"': "&quot;", "'": "&#x27;" };
function escapeHtml(s) {
  return s.replace(/[<>&"']/g, (c) => ESC[c]);
}

// ---------- Prism.js ----------
bench("Prism.js", () => Prism.highlight(sql, Prism.languages.sql, "sql"));

// ---------- Lezer (@codemirror/lang-sql + @lezer/highlight) ----------
{
  const parser = sqlLang({ dialect: SQLite }).language.parser;
  bench("Lezer (CodeMirror)", () => {
    const tree = parser.parse(sql);
    let html = "";
    let from = 0;
    highlightTree(tree, classHighlighter, (start, end, classes) => {
      if (start > from) html += escapeHtml(sql.slice(from, start));
      html += `<span class="${classes}">${escapeHtml(sql.slice(start, end))}</span>`;
      from = end;
    });
    if (from < sql.length) html += escapeHtml(sql.slice(from));
    return html;
  });
}

// ---------- tree-sitter (web-tree-sitter, JS WASM binding) ----------
{
  await Parser.init();
  const wasmBytes = readFileSync(join(__dirname, "tree-sitter-sql.wasm"));
  const lang = await Language.load(wasmBytes);
  const scm = readFileSync(
    join(__dirname, "tree-sitter-sql-highlights.scm"),
    "utf-8",
  );
  const query = new Query(lang, scm);
  const tsParser = new Parser();
  tsParser.setLanguage(lang);

  bench("tree-sitter (web-tree-sitter)", () => {
    const tree = tsParser.parse(sql);
    const captures = query.captures(tree.rootNode);
    let html = "";
    let from = 0;
    for (const cap of captures) {
      const start = cap.node.startIndex;
      const end = cap.node.endIndex;
      if (start < from) continue; // skip overlapping captures (simple resolver)
      if (start > from) html += escapeHtml(sql.slice(from, start));
      const cls = cap.name.replace(/\./g, " ");
      html += `<span class="${cls}">${escapeHtml(sql.slice(start, end))}</span>`;
      from = end;
    }
    if (from < sql.length) html += escapeHtml(sql.slice(from));
    tree.delete();
    return html;
  });
}

// ---------- Shiki (JS engine) ----------
{
  const highlighter = await createHighlighter({
    themes: ["github-dark"],
    langs: ["sql"],
    engine: createJavaScriptRegexEngine(),
  });
  bench("Shiki (JS engine)", () =>
    highlighter.codeToHtml(sql, { lang: "sql", theme: "github-dark" }),
  );
  highlighter.dispose();
}
