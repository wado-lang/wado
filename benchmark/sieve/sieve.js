// Sieve of Eratosthenes benchmark for JavaScript
// Counts primes up to LIMIT using the sieve algorithm
// Reference: π(10,000,000) = 664,579
//
// The sieve buffer is allocated once, outside the timed region, and reset to
// `true` at the start of each iteration. This keeps allocation and first-touch
// page faults out of the measurement so the timed loop reflects steady-state
// array traffic only, which is what makes the result stable across runs.
//
// Reports throughput (numbers sieved per second). The iteration count
// auto-calibrates so the timed loop runs for about a second.
//
// How to run:
//   mise run benchmark-sieve
//
// Or manually:
//   node benchmark/sieve/sieve.js

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

// Sieve over a caller-owned buffer, reused across iterations. Resets isPrime
// to all-true, then marks composites and counts primes.
function sieveCount(isPrime, limit) {
  // Reset the reused buffer to all-true.
  isPrime.fill(1);

  // 0 and 1 are not prime
  isPrime[0] = 0;
  isPrime[1] = 0;

  // Sieve: mark multiples of each prime as not prime
  for (let p = 2; p * p <= limit; p++) {
    if (isPrime[p]) {
      // Mark all multiples of p starting from p*p
      for (let multiple = p * p; multiple <= limit; multiple += p) {
        isPrime[multiple] = 0;
      }
    }
  }

  // Count primes
  let count = 0;
  for (let n = 0; n <= limit; n++) {
    if (isPrime[n]) {
      count++;
    }
  }

  return count;
}

function main() {
  const limit = 10000000;

  // Allocate the sieve buffer once, outside the timed region.
  const isPrime = new Uint8Array(limit + 1);

  // Warmup run: also first-touches the buffer so the timed loop is steady-state.
  let count = sieveCount(isPrime, limit);

  let n = 1;
  let elapsed = 0;
  for (;;) {
    const start = nowNs();
    for (let i = 0; i < n; i++) {
      count = sieveCount(isPrime, limit);
    }
    elapsed = nowNs() - start;
    if (elapsed >= TARGET_NS) break;
    const nx = nextIters(n, elapsed, TARGET_NS);
    if (nx <= n) break;
    n = nx;
  }

  console.log(`Sieve prime count up to ${limit}: ${count}`);
  printThroughput(limit, n, elapsed, "numbers");
}

main();
