// Prime counting benchmark for JavaScript
// Counts primes up to LIMIT using trial division
// Reference: π(1,000,000) = 78,498
//
// Reports throughput (numbers screened per second). The iteration count
// auto-calibrates so the timed loop runs for about a second.
//
// How to run:
//   mise run benchmark-count-prime
//
// Or manually:
//   node benchmark/count_prime/count_prime.js

const TARGET_NS = 1_000_000_000; // ~1s budget

function nowNs() {
  return Math.round(performance.now() * 1e6);
}

// Mirror of core:benchmark's calibration: scale n toward the target, capped at
// 100x growth per step and a hard ceiling.
function nextIters(n, elapsed, target) {
  const e = elapsed > 0 ? elapsed : 1;
  let est = Math.floor((n * target) / e);
  const hi = n * 100;
  if (est > hi) est = hi;
  if (est > 1_000_000_000) est = 1_000_000_000;
  if (est < 1) est = 1;
  return est;
}

function printThroughput(workPerIter, n, elapsedNs, unit) {
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
  console.log(`Throughput: ${rbuf}   (${perMs.toFixed(3)} ms/iter, ${n} iter)`);
}

function isPrime(n) {
  if (n < 2) {
    return false;
  }
  let d = 2;
  while (d * d <= n) {
    if (n % d === 0) {
      return false;
    }
    d += 1;
  }
  return true;
}

function countPrimes(limit) {
  let count = 0;
  let n = 2;
  while (n <= limit) {
    if (isPrime(n)) {
      count += 1;
    }
    n += 1;
  }
  return count;
}

const limit = 1000000;

// Warmup.
let count = countPrimes(limit);

let n = 1;
let elapsed = 0;
for (;;) {
  const start = nowNs();
  for (let i = 0; i < n; i++) {
    count = countPrimes(limit);
  }
  elapsed = nowNs() - start;
  if (elapsed >= TARGET_NS) break;
  const nx = nextIters(n, elapsed, TARGET_NS);
  if (nx <= n) break;
  n = nx;
}

console.log(`Prime count up to ${limit}: ${count}`);
printThroughput(limit, n, elapsed, "numbers");
