// Prime counting benchmark for JavaScript
// Counts primes up to LIMIT using trial division
// Reference: π(1,000,000) = 78,498
//
// How to run:
//   make benchmark-count-prime
//
// Or manually:
//   node benchmark/count_prime.js

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

const limit = 10000000;
const start = performance.now();
const count = countPrimes(limit);
const elapsed = performance.now() - start;

console.log(`Prime count up to ${limit}: ${count}`);
console.log(`Elapsed: ${elapsed.toFixed(2)} ms`);
