// Node.js JSON.parse benchmark for citm_catalog.json
// Comparison baseline for Wado's core:json deserialization.
//
// JSON data source: https://github.com/miloyip/nativejson-benchmark
// License: MIT

const fs = require("fs");
const path = require("path");

const jsonData = fs.readFileSync(
  path.join(__dirname, "citm_catalog.json"),
  "utf-8",
);
const iterations = 10;

console.log(`json-catalog: ${jsonData.length} bytes, ${iterations} iterations`);

const start = performance.now();
let totalEvents = 0;
let totalPerformances = 0;
for (let i = 0; i < iterations; i++) {
  const catalog = JSON.parse(jsonData);
  totalEvents += Object.keys(catalog.events).length;
  totalPerformances += catalog.performances.length;
}
const elapsed = performance.now() - start;

if (totalEvents !== 184 * iterations) throw new Error("assertion failed");
if (totalPerformances !== 243 * iterations)
  throw new Error("assertion failed");
console.log(`Parsed ${totalEvents} events, ${totalPerformances} performances`);
console.log(`Elapsed: ${elapsed.toFixed(3)} ms`);
