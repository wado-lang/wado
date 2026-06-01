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

const TARGET_NS = 1_000_000_000; // ~1s budget per highlighter

function nowNs() {
  return Math.round(performance.now() * 1e6);
}

function nextIters(n, elapsed, target) {
  const e = elapsed > 0 ? elapsed : 1;
  let est = Math.floor((n * target) / e);
  const hi = n * 100;
  if (est > hi) est = hi;
  if (est > 1_000_000_000) est = 1_000_000_000;
  if (est < 1) est = 1;
  return est;
}

function report(label, workPerIter, n, elapsedNs, unit) {
  const secs = elapsedNs / 1e9;
  const rate = secs > 0 ? (workPerIter * n) / secs : 0;
  const perMs = elapsedNs / n / 1e6;
  let rbuf;
  if (unit === "B") {
    if (rate >= 1e9) rbuf = `${(rate / 1e9).toFixed(2)} GB/s`;
    else if (rate >= 1e6) rbuf = `${(rate / 1e6).toFixed(2)} MB/s`;
    else if (rate >= 1e3) rbuf = `${(rate / 1e3).toFixed(2)} KB/s`;
    else rbuf = `${rate.toFixed(2)} B/s`;
  } else {
    if (rate >= 1e9) rbuf = `${(rate / 1e9).toFixed(2)} G ${unit}/s`;
    else if (rate >= 1e6) rbuf = `${(rate / 1e6).toFixed(2)} M ${unit}/s`;
    else if (rate >= 1e3) rbuf = `${(rate / 1e3).toFixed(2)} k ${unit}/s`;
    else rbuf = `${rate.toFixed(2)} ${unit}/s`;
  }
  console.log(`${label}: ${rbuf}   (${perMs.toFixed(3)} ms/iter, ${n} iter)`);
}

function bench(label, run) {
  console.log(`\n=== ${label} ===`);
  console.log(`syntax-highlight (${label}): ${sql.length} bytes`);

  const warm = run();
  if (!warm || warm.length === 0) {
    throw new Error(`${label}: warm-up produced empty output`);
  }

  let iters = 1;
  let elapsed = 0;
  let lastLen = 0;
  for (;;) {
    const start = nowNs();
    for (let i = 0; i < iters; i++) {
      lastLen = run().length;
    }
    elapsed = nowNs() - start;
    if (elapsed >= TARGET_NS) break;
    const nx = nextIters(iters, elapsed, TARGET_NS);
    if (nx <= iters) break;
    iters = nx;
  }

  if (lastLen === 0) {
    throw new Error(`${label}: produced empty output`);
  }
  report(label, sql.length, iters, elapsed, "B");
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
