// Float-to-string benchmark
// Converts 1M random f64 values to decimal strings using %.6f format.
// Uses a linear congruential generator for deterministic float sequence.
//
// Reports throughput (conversions per second). The iteration count
// auto-calibrates so the timed loop runs for about a second.
//
// How to run:
//   mise run benchmark-fts

#include <stdio.h>
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

static unsigned long g_byte_sum;

// Convert `n` LCG-derived f64 values to "%.6f" strings, returning the total
// byte count. Accumulates a byte checksum into a global so the conversions are
// not optimized away.
long total_bytes_run(int n) {
    unsigned int state = 42;
    char buf[32];
    long total_bytes = 0;
    unsigned long byte_sum = 0;

    for (int i = 0; i < n; i++) {
        state = (state * 1103515245u + 12345u) & 0x7FFFFFFFu;
        double x = (double)state / 2147483648.0;
        int len = snprintf(buf, sizeof(buf), "%.6f", x);
        total_bytes += len;
        for (int j = 0; j < len; j++) {
            byte_sum += (unsigned char)buf[j];
        }
    }

    g_byte_sum = byte_sum;
    return total_bytes;
}

int main(void) {
    const int n = 1000000;

    // Warmup.
    long total_bytes = total_bytes_run(n);

    long long iters = 1, elapsed = 0;
    for (;;) {
        long long start = now_ns();
        for (long long i = 0; i < iters; i++) {
            total_bytes = total_bytes_run(n);
        }
        elapsed = now_ns() - start;
        if (elapsed >= TARGET_NS) break;
        long long nx = next_iters(iters, elapsed, TARGET_NS);
        if (nx <= iters) break;
        iters = nx;
    }

    printf("fts: %d f64 conversions (%%.6f)\n", n);
    printf("Total bytes: %ld, byte sum: %lu\n", total_bytes, g_byte_sum);
    print_throughput((double)n, iters, elapsed, "conversions");
    return 0;
}
