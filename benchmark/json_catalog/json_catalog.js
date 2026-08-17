// Node.js JSON.parse benchmark for citm_catalog.json
// Comparison baseline for Wado's core:json deserialization.
//
// Reports deserialization throughput (MB/s). The iteration count
// auto-calibrates so the timed loop runs for about a second.
//
// JSON data source: https://github.com/miloyip/nativejson-benchmark
// License: MIT

const fs = require("fs");
const path = require("path");

const TARGET_NS = 1_000_000_000; // ~1s budget

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

// Calibrate `f` to run for about TARGET_NS, then report its throughput.
function bench(label, workPerIter, unit, f) {
  let result = f(); // warmup
  let iters = 1;
  let elapsed = 0;
  for (;;) {
    const start = nowNs();
    for (let i = 0; i < iters; i++) {
      result = f();
    }
    elapsed = nowNs() - start;
    if (elapsed >= TARGET_NS) break;
    const nx = nextIters(iters, elapsed, TARGET_NS);
    if (nx <= iters) break;
    iters = nx;
  }
  report(label, workPerIter, iters, elapsed, unit);
  return result;
}

// Raw bytes so the deserialize row starts where the Rust and Wado rows do.
// The object under Ser is parsed from a plainly-read string: decoding through
// TextDecoder instead leaves V8 a representation that halves stringify.
const jsonBytes = fs.readFileSync(path.join(__dirname, "citm_catalog.json"));
const size = jsonBytes.length;
const decoder = new TextDecoder();
const catalogObj = JSON.parse(fs.readFileSync(path.join(__dirname, "citm_catalog.json"), "utf-8"));

console.log(`json-catalog: ${size} bytes`);

// Encoded, because `String.length` counts UTF-16 units rather than UTF-8 bytes,
// and the other two rows produce UTF-8 bytes.
const encoder = new TextEncoder();
bench("Ser", size, "B", () => encoder.encode(JSON.stringify(catalogObj)).length);

const counts = bench("De", size, "B", () => {
  const catalog = JSON.parse(decoder.decode(jsonBytes));
  return [Object.keys(catalog.events).length, catalog.performances.length];
});

if (counts[0] !== 184) throw new Error("assertion failed");
if (counts[1] !== 243) throw new Error("assertion failed");
console.log(`Round-tripped ${counts[0]} events, ${counts[1]} performances per iteration`);
