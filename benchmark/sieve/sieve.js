// Sieve of Eratosthenes benchmark for JavaScript
// Counts primes up to LIMIT using the sieve algorithm
// Reference: π(10,000,000) = 664,579
//
// The sieve buffer is allocated once, outside the timed region, and reset to
// `true` at the start of each iteration. This keeps allocation and first-touch
// page faults out of the measurement so the timed loop reflects steady-state
// array traffic only, which is what makes the result stable across runs.
//
// How to run:
//   mise run benchmark-sieve
//
// Or manually:
//   node benchmark/sieve/sieve.js

const ITERATIONS = 10;

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

    const start = performance.now();
    for (let i = 0; i < ITERATIONS; i++) {
        count = sieveCount(isPrime, limit);
    }
    const end = performance.now();

    const totalMs = Math.round(end - start);
    console.log(`Sieve prime count up to ${limit}: ${count}`);
    console.log(`Elapsed: ${totalMs} ms total (${ITERATIONS} iterations, ${Math.round(totalMs / ITERATIONS)} ms/iter)`);
}

main();
