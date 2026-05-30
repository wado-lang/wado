// Sieve of Eratosthenes benchmark for JavaScript
// Counts primes up to LIMIT using the sieve algorithm
// Reference: π(100,000,000) = 5,761,455
//
// How to run:
//   mise run benchmark-sieve
//
// Or manually:
//   node benchmark/sieve.js

function sieveCount(limit) {
    // Use typed array for better performance
    const isPrime = new Uint8Array(limit + 1);
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
    const limit = 100000000;

    const start = performance.now();
    const count = sieveCount(limit);
    const end = performance.now();

    const elapsedMs = Math.round(end - start);

    console.log(`Sieve prime count up to ${limit}: ${count}`);
    console.log(`Elapsed: ${elapsedMs} ms`);
}

main();
