// Mandelbrot set benchmark for C
// Counts total iterations across a grid of points
// Real-world use case: fractal rendering, GPU benchmarks
//
// How to run:
//   make benchmark-mandelbrot
//
// Or manually:
//   cc -O3 -ffp-contract=off -o benchmark/mandelbrot_c benchmark/mandelbrot.c
//   ./benchmark/mandelbrot_c
//
// Note: -ffp-contract=off disables FMA (fused multiply-add) to ensure
// consistent IEEE 754 behavior with JavaScript and WebAssembly.

#include <stdio.h>
#include <stdint.h>
#include <time.h>

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

int main() {
    volatile int width = 1024;
    volatile int height = 768;
    volatile int max_iter = 256;

    // Mandelbrot region: x in [-2.5, 1.0], y in [-1.0, 1.0]
    double x_min = -2.5;
    double x_max = 1.0;
    double y_min = -1.0;
    double y_max = 1.0;

    double dx = (x_max - x_min) / (double)width;
    double dy = (y_max - y_min) / (double)height;

    clock_t start = clock();

    int64_t total_iterations = 0;

    for (int py = 0; py < height; py++) {
        double cy = y_min + (double)py * dy;

        for (int px = 0; px < width; px++) {
            double cx = x_min + (double)px * dx;
            int iter = mandelbrot_iterations(cx, cy, max_iter);
            total_iterations += iter;
        }
    }

    clock_t end = clock();
    double elapsed_ms = (double)(end - start) / CLOCKS_PER_SEC * 1000.0;

    printf("Mandelbrot %dx%d, max_iter=%d\n", width, height, max_iter);
    printf("Total iterations: %lld\n", (long long)total_iterations);
    printf("Elapsed: %.2f ms\n", elapsed_ms);

    return 0;
}
