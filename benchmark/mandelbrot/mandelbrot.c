// Mandelbrot set benchmark for C
// Counts total iterations across a grid of points
// Real-world use case: fractal rendering, GPU benchmarks
//
// Reports throughput (pixels rendered per second). The iteration count
// auto-calibrates so the timed loop runs for about a second.
//
// How to run:
//   mise run benchmark-mandelbrot
//
// Or manually:
//   cc -O3 -ffp-contract=off -o benchmark/mandelbrot/mandelbrot_c benchmark/mandelbrot/mandelbrot.c
//   ./benchmark/mandelbrot/mandelbrot_c
//
// Note: -ffp-contract=off disables FMA (fused multiply-add) to ensure
// consistent IEEE 754 behavior with JavaScript and WebAssembly.

#include <stdio.h>
#include <stdint.h>
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

int mandelbrot_iterations(double cx, double cy, int max_iter) {
    double x = 0.0;
    double y = 0.0;
    int iter = 0;

    while (iter < max_iter) {
        double x2 = x * x;
        double y2 = y * y;

        if (x2 + y2 > 4.0) {
            return iter;
        }

        double xy = x * y;
        x = x2 - y2 + cx;
        y = 2.0 * xy + cy;
        iter += 1;
    }

    return max_iter;
}

int64_t mandelbrot_total(int width, int height, int max_iter) {
    // Mandelbrot region: x in [-2.5, 1.0], y in [-1.0, 1.0]
    double x_min = -2.5;
    double x_max = 1.0;
    double y_min = -1.0;
    double y_max = 1.0;

    double dx = (x_max - x_min) / (double)width;
    double dy = (y_max - y_min) / (double)height;

    int64_t total_iterations = 0;

    for (int py = 0; py < height; py++) {
        double cy = y_min + (double)py * dy;

        for (int px = 0; px < width; px++) {
            double cx = x_min + (double)px * dx;
            total_iterations += mandelbrot_iterations(cx, cy, max_iter);
        }
    }

    return total_iterations;
}

int main() {
    volatile int width = 1024;
    volatile int height = 768;
    volatile int max_iter = 256;

    // Warmup.
    int64_t total = mandelbrot_total(width, height, max_iter);

    long long n = 1, elapsed = 0;
    for (;;) {
        long long start = now_ns();
        for (long long i = 0; i < n; i++) {
            total = mandelbrot_total(width, height, max_iter);
        }
        elapsed = now_ns() - start;
        if (elapsed >= TARGET_NS) break;
        long long nx = next_iters(n, elapsed, TARGET_NS);
        if (nx <= n) break;
        n = nx;
    }

    printf("Mandelbrot %dx%d, max_iter=%d\n", width, height, max_iter);
    printf("Total iterations: %lld\n", (long long)total);
    print_throughput((double)width * (double)height, n, elapsed, "px");

    return 0;
}
