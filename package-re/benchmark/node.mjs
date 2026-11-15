// Node's `RegExp` over the cases `re.wado` runs: the same patterns, read with
// the `u` flag as Re reads them, on the same text.
//
// How to run:
//   node node.mjs

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const INPUT = readFileSync(join(__dirname, "input.txt"), "utf-8");

// Backtracking doubles with each `x`, so this length is what keeps one
// iteration of Node's inside the time budget. Re's cost grows with the length.
const PATHOLOGICAL = "x".repeat(24);

const TARGET_NS = 1_000_000_000; // ~1s budget per case

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

function report(label, size, n, elapsedNs) {
  const secs = elapsedNs / 1e9;
  const rate = secs > 0 ? (size * n) / secs : 0;
  const perMs = elapsedNs / n / 1e6;
  let rbuf;
  if (rate >= 1e9) rbuf = `${(rate / 1e9).toFixed(2)} GB/s`;
  else if (rate >= 1e6) rbuf = `${(rate / 1e6).toFixed(2)} MB/s`;
  else if (rate >= 1e3) rbuf = `${(rate / 1e3).toFixed(2)} KB/s`;
  else rbuf = `${rate.toFixed(2)} B/s`;
  console.log(`  ${label}: ${rbuf}   (${perMs.toFixed(3)} ms/iter, ${n} iter)`);
}

function countMatches(regex, text) {
  regex.lastIndex = 0;
  let n = 0;
  while (regex.exec(text) !== null) n++;
  return n;
}

// What `literal` counts, by `indexOf` and no regular expression.
function countFinds(text, needle) {
  let n = 0;
  for (let at = text.indexOf(needle); at !== -1; at = text.indexOf(needle, at + needle.length)) n++;
  return n;
}

// The sum of every timestamp's hour, so that each match's groups are read.
function sumHours(regex, text) {
  regex.lastIndex = 0;
  let sum = 0;
  for (let m; (m = regex.exec(text)) !== null; ) sum += Number(m[4]);
  return sum;
}

function bench(label, text, expected, run) {
  const warm = run();
  if (warm !== expected) {
    throw new Error(`${label}: expected ${expected}, got ${warm}`);
  }
  let iters = 1;
  let elapsed = 0;
  for (;;) {
    const start = nowNs();
    for (let i = 0; i < iters; i++) run();
    elapsed = nowNs() - start;
    if (elapsed >= TARGET_NS) break;
    const nx = nextIters(iters, elapsed, TARGET_NS);
    if (nx <= iters) break;
    iters = nx;
  }
  report(label, Buffer.byteLength(text), iters, elapsed);
}

const literal = /Sherlock Holmes/gu;
const literalIgnoreCase = /(?i:sherlock holmes)/gu;
const email = /[\w.+-]+@[\w-]+(?:\.[\w-]+)+/gu;
const uri = /[a-z][a-z0-9+.-]*:\/\/[^\s\/?#]+[^\s?#]*(?:\?[^\s#]*)?(?:#\S*)?/gu;
const timestamp = /(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})Z/gu;
const pathological = /(?:x+x+)+y/u;

console.log(`re (Node.js RegExp): ${Buffer.byteLength(INPUT)} bytes`);
bench("literal", INPUT, 223, () => countMatches(literal, INPUT));
bench("literal-find", INPUT, 223, () => countFinds(INPUT, "Sherlock Holmes"));
bench("literal-ignore-case", INPUT, 452, () => countMatches(literalIgnoreCase, INPUT));
bench("email", INPUT, 176, () => countMatches(email, INPUT));
bench("uri", INPUT, 176, () => countMatches(uri, INPUT));
bench("timestamp-captures", INPUT, 1904, () => sumHours(timestamp, INPUT));
bench("pathological", PATHOLOGICAL, 0, () => (pathological.test(PATHOLOGICAL) ? 1 : 0));
