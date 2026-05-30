// Float-to-string benchmark
// Converts 5M random f64 values to decimal strings using %.6f format.
// Uses a linear congruential generator for deterministic float sequence.
//
// How to run:
//   mise run benchmark-fts

#include <stdio.h>
#include <time.h>

int main(void) {
    const int n = 5000000;
    unsigned int state = 42;
    char buf[32];
    long total_bytes = 0;
    unsigned long byte_sum = 0;

    struct timespec t0, t1;
    clock_gettime(CLOCK_MONOTONIC, &t0);

    for (int i = 0; i < n; i++) {
        state = (state * 1103515245u + 12345u) & 0x7FFFFFFFu;
        double x = (double)state / 2147483648.0;
        int len = snprintf(buf, sizeof(buf), "%.6f", x);
        total_bytes += len;
        for (int j = 0; j < len; j++) {
            byte_sum += (unsigned char)buf[j];
        }
    }

    clock_gettime(CLOCK_MONOTONIC, &t1);
    long elapsed_ms = (t1.tv_sec - t0.tv_sec) * 1000 +
                      (t1.tv_nsec - t0.tv_nsec) / 1000000;

    printf("fts: 5000000 f64 conversions (%%.6f)\n");
    printf("Total bytes: %ld, byte sum: %lu\n", total_bytes, byte_sum);
    printf("Elapsed: %ld ms\n", elapsed_ms);
    return 0;
}
