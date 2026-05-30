// Sieve of Eratosthenes benchmark for C
// Counts primes up to LIMIT using the sieve algorithm
// Reference: π(100,000,000) = 5,761,455
//
// How to run:
//   mise run benchmark-sieve
//
// Or manually:
//   clang -O3 -o benchmark/sieve_c benchmark/sieve.c
//   ./benchmark/sieve_c

#include <stdio.h>
#include <stdlib.h>
#include <stdbool.h>
#include <time.h>

int sieve_count(int limit) {
    bool *is_prime = malloc((limit + 1) * sizeof(bool));
    if (!is_prime) {
        fprintf(stderr, "Memory allocation failed\n");
        exit(1);
    }

    // Initialize all to true
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

    free(is_prime);
    return count;
}

int main() {
    int limit = 100000000;

    struct timespec start, end;
    clock_gettime(CLOCK_MONOTONIC, &start);

    int count = sieve_count(limit);

    clock_gettime(CLOCK_MONOTONIC, &end);

    long elapsed_ns = (end.tv_sec - start.tv_sec) * 1000000000L + (end.tv_nsec - start.tv_nsec);
    long elapsed_ms = elapsed_ns / 1000000;

    printf("Sieve prime count up to %d: %d\n", limit, count);
    printf("Elapsed: %ld ms\n", elapsed_ms);

    return 0;
}
