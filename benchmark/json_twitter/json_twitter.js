// Node.js JSON.parse benchmark for twitter.json
// Comparison baseline for Wado's core:json deserialization.
//
// JSON data source: https://github.com/miloyip/nativejson-benchmark
// License: MIT

const fs = require("fs");
const path = require("path");

const jsonData = fs.readFileSync(
  path.join(__dirname, "twitter.json"),
  "utf-8",
);
const iterations = 10;

console.log(`json-twitter: ${jsonData.length} bytes, ${iterations} iterations`);

const start = performance.now();
let count = 0;
for (let i = 0; i < iterations; i++) {
  const resp = JSON.parse(jsonData);
  count += resp.statuses.length;
}
const elapsed = performance.now() - start;

if (count !== 100 * iterations) throw new Error("assertion failed");
console.log(`Parsed ${count} total statuses`);
console.log(`Elapsed: ${elapsed.toFixed(3)} ms`);
