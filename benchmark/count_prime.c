// Prime counting benchmark for C
// Counts primes up to LIMIT using trial division
// Reference: π(1,000,000) = 78,498
//
// How to run:
//   make benchmark-count-prime
//
// Or manually:
//   cc -O3 -o benchmark/count_prime benchmark/count_prime.c
//   ./benchmark/count_prime

#include <stdio.h>
#include <stdbool.h>
#include <time.h>

bool is_prime(int n) {
    if (n < 2) {
        return false;
    }
    int d = 2;
    while (d * d <= n) {
        if (n % d == 0) {
            return false;
        }
        d += 1;
    }
    return true;
}

int count_primes(int limit) {
    int count = 0;
    int n = 2;
    while (n <= limit) {
        if (is_prime(n)) {
            count += 1;
        }
        n += 1;
    }
    return count;
}

int main() {
    volatile int limit = 10000000;  // volatile to prevent constant folding

    clock_t start = clock();
    int count = count_primes(limit);
    clock_t end = clock();

    double elapsed_ms = (double)(end - start) / CLOCKS_PER_SEC * 1000.0;

    printf("Prime count up to %d: %d\n", limit, count);
    printf("Elapsed: %.2f ms\n", elapsed_ms);

    return 0;
}
