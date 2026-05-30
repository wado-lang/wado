// Node.js JSON.parse benchmark for canada.json
// Comparison baseline for Wado's core:json deserialization.
//
// JSON data source: https://github.com/miloyip/nativejson-benchmark
// License: MIT

const fs = require("fs");
const path = require("path");

const jsonData = fs.readFileSync(
  path.join(__dirname, "canada.json"),
  "utf-8",
);
const iterations = 10;

console.log(`json-canada: ${jsonData.length} bytes, ${iterations} iterations`);

const start = performance.now();
let totalPoints = 0;
for (let i = 0; i < iterations; i++) {
  const fc = JSON.parse(jsonData);
  for (const feat of fc.features) {
    for (const ring of feat.geometry.coordinates) {
      totalPoints += ring.length;
    }
  }
}
const elapsed = performance.now() - start;

if (totalPoints !== 55563 * iterations) throw new Error("assertion failed");
console.log(`Parsed ${totalPoints} total coordinate points`);
console.log(`Elapsed: ${elapsed.toFixed(3)} ms`);
