// Prime counting benchmark for C
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
//   cc -O3 -o benchmark/count_prime/count_prime_c benchmark/count_prime/count_prime.c
//   ./benchmark/count_prime/count_prime_c

#include <stdio.h>
#include <stdbool.h>
#include <string.h>
#include <time.h>

#define TARGET_NS 1000000000LL  // ~1s budget

static long long now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (long long)t.tv_sec * 1000000000LL + t.tv_nsec;
}

// Mirror of core:benchmark's calibration: scale n toward the target, capped at
// 100x growth per step and a hard ceiling.
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
    volatile int limit = 1000000;  // volatile to prevent constant folding

    // Warmup.
    int count = count_primes(limit);

    long long n = 1, elapsed = 0;
    for (;;) {
        long long start = now_ns();
        for (long long i = 0; i < n; i++) {
            count = count_primes(limit);
        }
        elapsed = now_ns() - start;
        if (elapsed >= TARGET_NS) break;
        long long nx = next_iters(n, elapsed, TARGET_NS);
        if (nx <= n) break;
        n = nx;
    }

    printf("Prime count up to %d: %d\n", limit, count);
    print_throughput((double)limit, n, elapsed, "numbers");

    return 0;
}
