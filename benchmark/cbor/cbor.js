// @msgpack/msgpack ser/de benchmark over twitter.json.
//
// MessagePack is not CBOR, but it is the closest widely-used compact binary
// codec in the JS ecosystem, so it serves as a speed reference for the Wado
// core:cbor benchmark. The JSON is parsed once into a plain object, then both
// phases operate on byte buffers (Uint8Array):
//   ser: object     -> msgpack bytes  (encode)
//   de:  msgpack bytes -> object      (decode)
// Throughput is reported over the msgpack payload size.
//
// JSON data source: https://github.com/miloyip/nativejson-benchmark
// License: MIT

import fs from "node:fs";
import { encode, decode } from "@msgpack/msgpack";

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

const jsonPath = new URL("../json_twitter/twitter.json", import.meta.url);
const jsonText = fs.readFileSync(jsonPath, "utf-8");
const jsonSize = Buffer.byteLength(jsonText, "utf-8");
const obj = JSON.parse(jsonText);

const encoded = encode(obj);
console.log(
  `cbor-twitter: ${jsonSize} bytes JSON -> ${encoded.length} bytes msgpack payload`,
);

// Throughput is reported over the JSON source size (shared denominator across
// implementations), not the codec payload size.
bench("Ser", jsonSize, "B", () => encode(obj).length);

const count = bench("De", jsonSize, "B", () => {
  const r = decode(encoded);
  return r.statuses.length;
});

if (count !== 100) throw new Error("assertion failed");
console.log(`Round-tripped ${count} statuses per iteration`);
