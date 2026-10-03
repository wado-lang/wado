// Prism.js highlighting the same page as the Gale arm. Its `markup` grammar
// inlines CSS and JavaScript for `<style>`, `<script>`, `style="..."` and
// `on*="..."`.
//
// How to run:
//   node highlighters.js
// (Run `npm install` here first.)

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import Prism from "prismjs";
import "prismjs/components/prism-css.js";
import "prismjs/components/prism-javascript.js";

const __dirname = dirname(fileURLToPath(import.meta.url));
const html = readFileSync(join(__dirname, "input.html"), "utf-8");
const size = Buffer.byteLength(html);

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

function report(label, n, elapsedNs) {
  const secs = elapsedNs / 1e9;
  const rate = secs > 0 ? (size * n) / secs : 0;
  const perMs = elapsedNs / n / 1e6;
  let rbuf;
  if (rate >= 1e9) rbuf = `${(rate / 1e9).toFixed(2)} GB/s`;
  else if (rate >= 1e6) rbuf = `${(rate / 1e6).toFixed(2)} MB/s`;
  else if (rate >= 1e3) rbuf = `${(rate / 1e3).toFixed(2)} KB/s`;
  else rbuf = `${rate.toFixed(2)} B/s`;
  console.log(`${label}: ${rbuf}   (${perMs.toFixed(3)} ms/iter, ${n} iter)`);
}

// `injected` is a fragment of the output that only an embedded language's
// highlighting produces, so an arm that left the regions as text fails.
function bench(label, injected, run) {
  console.log(`\n=== ${label} ===`);
  console.log(`gale-highlight-html (${label}): ${size} bytes`);

  const warm = run();
  for (const fragment of injected) {
    if (!warm.includes(fragment)) {
      throw new Error(`${label}: output lacks ${fragment}`);
    }
  }

  let iters = 1;
  let elapsed = 0;
  for (;;) {
    const start = nowNs();
    for (let i = 0; i < iters; i++) {
      run();
    }
    elapsed = nowNs() - start;
    if (elapsed >= TARGET_NS) break;
    const nx = nextIters(iters, elapsed, TARGET_NS);
    if (nx <= iters) break;
    iters = nx;
  }
  report(label, iters, elapsed);
}

// ---------- Prism.js ----------
bench(
  "Prism.js",
  ['<span class="token keyword">class</span>', '<span class="token property">margin</span>'],
  () => Prism.highlight(html, Prism.languages.markup, "markup"),
);

