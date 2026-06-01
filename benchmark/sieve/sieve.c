// Sieve of Eratosthenes benchmark for C
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
//   cc -O3 -o benchmark/sieve/sieve_c benchmark/sieve/sieve.c
//   ./benchmark/sieve/sieve_c

#include <stdio.h>
#include <stdlib.h>
#include <stdbool.h>
#include <string.h>
#include <time.h>

#define TARGET_NS 1000000000LL  // ~1s budget

static long long now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (long long)t.tv_sec * 1000000000LL + t.tv_nsec;
}

static long long next_iters(long long n, long long elapsed, long long target) {
    long long e = elapsed > 0 ? elapsed : 1;
    long long est = n * target / e;
    long long hi = n * 100;
    if (est > hi) est = hi;
    if (est > 1000000000LL) est = 1000000000LL;
    if (est < 1) est = 1;
    return est;
}

static void print_throughput(double work_per_iter, long long n, long long elapsed_ns, const char *unit) {
    double secs = (double)elapsed_ns / 1e9;
    double rate = secs > 0.0 ? (work_per_iter * (double)n) / secs : 0.0;
    double per_ms = (double)elapsed_ns / (double)n / 1e6;
    char rbuf[64];
    if (strcmp(unit, "B") == 0) {
        if (rate >= 1e9) snprintf(rbuf, sizeof rbuf, "%.2f GB/s", rate / 1e9);
        else if (rate >= 1e6) snprintf(rbuf, sizeof rbuf, "%.2f MB/s", rate / 1e6);
        else if (rate >= 1e3) snprintf(rbuf, sizeof rbuf, "%.2f KB/s", rate / 1e3);
        else snprintf(rbuf, sizeof rbuf, "%.2f B/s", rate);
    } else {
        if (rate >= 1e9) snprintf(rbuf, sizeof rbuf, "%.2f G %s/s", rate / 1e9, unit);
        else if (rate >= 1e6) snprintf(rbuf, sizeof rbuf, "%.2f M %s/s", rate / 1e6, unit);
        else if (rate >= 1e3) snprintf(rbuf, sizeof rbuf, "%.2f k %s/s", rate / 1e3, unit);
        else snprintf(rbuf, sizeof rbuf, "%.2f %s/s", rate, unit);
    }
    printf("Throughput: %s   (%.3f ms/iter, %lld iter)\n", rbuf, per_ms, n);
}

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

    long long n = 1, elapsed = 0;
    for (;;) {
        long long start = now_ns();
        for (long long i = 0; i < n; i++) {
            count = sieve_count(is_prime, limit);
        }
        elapsed = now_ns() - start;
        if (elapsed >= TARGET_NS) break;
        long long nx = next_iters(n, elapsed, TARGET_NS);
        if (nx <= n) break;
        n = nx;
    }

    free(is_prime);

    printf("Sieve prime count up to %d: %d\n", limit, count);
    print_throughput((double)limit, n, elapsed, "numbers");

    return 0;
}
