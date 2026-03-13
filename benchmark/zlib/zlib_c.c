// zlib C benchmark for comparison with Wado and zlib-rs
//
// Compresses and decompresses 100KB of patterned data x10 iterations,
// using the same data pattern as zlib_bench.wado and zlib_rs.rs.
//
// Compiled to Wasm with:
//   clang --target=wasm32-wasi -O3 ...

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include "../../wasm-size/zlib/zlib-1.3.1/zlib.h"

static long long now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

int main(void) {
    int size = 100000;
    int iterations = 10;

    // Generate 100KB of patterned data: bytes (i % 256)
    unsigned char *data = (unsigned char *)malloc(size);
    for (int i = 0; i < size; i++) {
        data[i] = (unsigned char)(i % 256);
    }

    printf("zlib %d bytes x %d iterations\n", size, iterations);

    // Benchmark compression (10 iterations)
    uLong bound = compressBound((uLong)size);
    unsigned char *compressed = (unsigned char *)malloc(bound);
    uLong compressed_len = 0;

    long long t0 = now_ns();
    for (int iter = 0; iter < iterations; iter++) {
        uLong dest_len = bound;
        int rc = compress2(compressed, &dest_len, data, (uLong)size, 6);
        if (rc != Z_OK) {
            fprintf(stderr, "compress2 failed: %d\n", rc);
            return 1;
        }
        compressed_len = dest_len;
    }
    long long t1 = now_ns();

    long long compress_ns = t1 - t0;
    long long compress_ms = compress_ns / 1000000;
    long long compress_us_rem = (compress_ns / 1000) % 1000;
    printf("Compressed: %d -> %lu bytes\n", size, (unsigned long)compressed_len);
    printf("Compress: %lld.%03lld ms\n", compress_ms, compress_us_rem);

    // Benchmark decompression (10 iterations)
    unsigned char *decompressed = (unsigned char *)malloc(size);

    long long t2 = now_ns();
    for (int iter = 0; iter < iterations; iter++) {
        uLong dest_len = (uLong)size;
        int rc = uncompress(decompressed, &dest_len, compressed, compressed_len);
        if (rc != Z_OK) {
            fprintf(stderr, "uncompress failed: %d\n", rc);
            return 1;
        }
    }
    long long t3 = now_ns();

    long long decompress_ns = t3 - t2;
    long long decompress_ms = decompress_ns / 1000000;
    long long decompress_us_rem = (decompress_ns / 1000) % 1000;
    printf("Decompress: %lld.%03lld ms\n", decompress_ms, decompress_us_rem);

    // Verify round-trip
    if (memcmp(data, decompressed, size) != 0) {
        fprintf(stderr, "ERROR: decompressed data mismatch\n");
        return 1;
    }

    long long total_ns = compress_ns + decompress_ns;
    long long total_ms = total_ns / 1000000;
    long long total_us_rem = (total_ns / 1000) % 1000;
    printf("Elapsed: %lld.%03lld ms\n", total_ms, total_us_rem);

    free(data);
    free(compressed);
    free(decompressed);
    return 0;
}
