// JavaScript syntax highlighter benchmarks for SQL.
// Comparison baselines for the Gale-generated SQLite highlighter.
//
// Highlighters:
//   - highlight.js  (regex-based, most popular on npm)
//   - Prism.js      (regex-based, lightweight; often the fastest of the three)
//   - Shiki         (TextMate grammars; the "VSCode-quality" reference)
//
// For Shiki we use its JavaScript regex engine. The Oniguruma (WASM)
// engine is omitted because it is roughly 2.5x slower than the JS
// engine on SQL while producing byte-identical output.
//
// How to run:
//   node syntax_highlight.js
// (Run `npm install` here first.)

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import hljs from "highlight.js/lib/core";
import hljsSql from "highlight.js/lib/languages/sql";

import Prism from "prismjs";
import "prismjs/components/prism-sql.js";

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
  console.log(
    `Per iteration: ${piInt}.${String(piFrac).padStart(3, "0")} us`,
  );
}

function bench(label, run) {
  console.log(`\n=== ${label} ===`);
  console.log(
    `syntax-highlight (${label}): ${sql.length} bytes, ${ITERATIONS} iterations`,
  );

  // Warm up.
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

// ---------- highlight.js ----------
hljs.registerLanguage("sql", hljsSql);
bench("highlight.js", () => hljs.highlight(sql, { language: "sql" }).value);

// ---------- Prism.js ----------
bench("Prism.js", () => Prism.highlight(sql, Prism.languages.sql, "sql"));

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
