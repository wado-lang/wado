// Node.js zlib benchmark for comparison with Wado's core:zlib.
//
// Compresses and decompresses twitter.json (~631KB) using Node's built-in
// zlib (deflate level 6, matching zlib-rs and core:zlib). Reports compression
// and decompression throughput (MB/s of the original data). Each phase
// auto-calibrates its iteration count to run for about a second.

const fs = require("fs");
const path = require("path");
const zlib = require("zlib");

const TARGET_NS = 1_000_000_000; // ~1s budget per phase

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

const data = fs.readFileSync(path.join(__dirname, "..", "json_twitter", "twitter.json"));
const size = data.length;

console.log(`zlib ${size} bytes`);

const compressed = bench("Compress", size, "B", () => zlib.deflateSync(data, { level: 6 }));

console.log(`Compressed: ${size} -> ${compressed.length} bytes`);

const decompressed = bench("Decompress", size, "B", () => zlib.inflateSync(compressed));

if (decompressed.length !== size) throw new Error("decompressed size mismatch");
