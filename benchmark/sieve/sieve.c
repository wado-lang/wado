// Sieve of Eratosthenes benchmark for C
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
//   clang -O3 -o benchmark/sieve/sieve_c benchmark/sieve/sieve.c
//   ./benchmark/sieve/sieve_c

#include <stdio.h>
#include <stdlib.h>
#include <stdbool.h>
#include <time.h>

#define ITERATIONS 10

// Sieve over a caller-owned buffer, reused across iterations. Resets is_prime
// to all-true, then marks composites and counts primes.
int sieve_count(bool *is_prime, int limit) {
    // Reset the reused buffer to all-true.
    for (int i = 0; i <= limit; i++) {
        is_prime[i] = true;
    }

    // 0 and 1 are not prime
    is_prime[0] = false;
    is_prime[1] = false;

    // Sieve: mark multiples of each prime as not prime
    for (int p = 2; p * p <= limit; p++) {
        if (is_prime[p]) {
            // Mark all multiples of p starting from p*p
            for (int multiple = p * p; multiple <= limit; multiple += p) {
                is_prime[multiple] = false;
            }
        }
    }

    // Count primes
    int count = 0;
    for (int n = 0; n <= limit; n++) {
        if (is_prime[n]) {
            count++;
        }
    }

    return count;
}

int main() {
    int limit = 10000000;

    // Allocate the sieve buffer once, outside the timed region.
    bool *is_prime = malloc((limit + 1) * sizeof(bool));
    if (!is_prime) {
        fprintf(stderr, "Memory allocation failed\n");
        exit(1);
    }

    // Warmup run: also first-touches every page so the timed loop is steady-state.
    int count = sieve_count(is_prime, limit);

    struct timespec start, end;
    clock_gettime(CLOCK_MONOTONIC, &start);

    for (int i = 0; i < ITERATIONS; i++) {
        count = sieve_count(is_prime, limit);
    }

    clock_gettime(CLOCK_MONOTONIC, &end);

    long elapsed_ns = (end.tv_sec - start.tv_sec) * 1000000000L + (end.tv_nsec - start.tv_nsec);
    long elapsed_ms = elapsed_ns / 1000000;

    free(is_prime);

    printf("Sieve prime count up to %d: %d\n", limit, count);
    printf("Elapsed: %ld ms total (%d iterations, %ld ms/iter)\n",
           elapsed_ms, ITERATIONS, elapsed_ms / ITERATIONS);

    return 0;
}
