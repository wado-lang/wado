// zlib C benchmark for comparison with Wado and zlib-rs
//
// Compresses and decompresses twitter.json (~631KB) with zlib (deflate level 6,
// matching zlib-rs and core:zlib). Reports compression and decompression
// throughput (MB/s of the original data). Each phase auto-calibrates its
// iteration count to run for about a second.
//
// Compiled to Wasm with:
//   clang --target=wasm32-wasi -O3 ...

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include "../../wasm-size/zlib/zlib-1.3.1/zlib.h"

#define TARGET_NS 1000000000LL  // ~1s budget per phase

static long long now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000000000LL + ts.tv_nsec;
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

static void report(const char *label, double work_per_iter, long long n, long long elapsed_ns) {
    double secs = (double)elapsed_ns / 1e9;
    double rate = secs > 0.0 ? (work_per_iter * (double)n) / secs : 0.0;
    double per_ms = (double)elapsed_ns / (double)n / 1e6;
    char rbuf[64];
    if (rate >= 1e9) snprintf(rbuf, sizeof rbuf, "%.2f GB/s", rate / 1e9);
    else if (rate >= 1e6) snprintf(rbuf, sizeof rbuf, "%.2f MB/s", rate / 1e6);
    else if (rate >= 1e3) snprintf(rbuf, sizeof rbuf, "%.2f KB/s", rate / 1e3);
    else snprintf(rbuf, sizeof rbuf, "%.2f B/s", rate);
    printf("%s: %s   (%.3f ms/iter, %lld iter)\n", label, rbuf, per_ms, n);
}

static unsigned char *read_file(const char *path, size_t *out_size) {
    FILE *f = fopen(path, "rb");
    if (!f) {
        fprintf(stderr, "failed to open %s\n", path);
        exit(1);
    }
    fseek(f, 0, SEEK_END);
    long len = ftell(f);
    fseek(f, 0, SEEK_SET);
    unsigned char *buf = (unsigned char *)malloc(len);
    if (fread(buf, 1, len, f) != (size_t)len) {
        fprintf(stderr, "failed to read %s\n", path);
        exit(1);
    }
    fclose(f);
    *out_size = (size_t)len;
    return buf;
}

typedef struct {
    unsigned char *data;
    size_t size;
    unsigned char *compressed;
    uLong bound;
    uLong compressed_len;
    unsigned char *decompressed;
} bench_ctx;

static void run_compress(bench_ctx *c) {
    uLong dest_len = c->bound;
    int rc = compress2(c->compressed, &dest_len, c->data, (uLong)c->size, 6);
    if (rc != Z_OK) {
        fprintf(stderr, "compress2 failed: %d\n", rc);
        exit(1);
    }
    c->compressed_len = dest_len;
}

static void run_decompress(bench_ctx *c) {
    uLong dest_len = (uLong)c->size;
    int rc = uncompress(c->decompressed, &dest_len, c->compressed, c->compressed_len);
    if (rc != Z_OK) {
        fprintf(stderr, "uncompress failed: %d\n", rc);
        exit(1);
    }
}

// Calibrate `f` to run for about TARGET_NS, then report its throughput.
static void bench(const char *label, double work_per_iter, void (*f)(bench_ctx *), bench_ctx *ctx) {
    f(ctx);  // warmup
    long long n = 1, elapsed = 0;
    for (;;) {
        long long start = now_ns();
        for (long long i = 0; i < n; i++) {
            f(ctx);
        }
        elapsed = now_ns() - start;
        if (elapsed >= TARGET_NS) break;
        long long nx = next_iters(n, elapsed, TARGET_NS);
        if (nx <= n) break;
        n = nx;
    }
    report(label, work_per_iter, n, elapsed);
}

int main(void) {
    bench_ctx ctx;
    ctx.data = read_file("json_twitter/twitter.json", &ctx.size);
    ctx.bound = compressBound((uLong)ctx.size);
    ctx.compressed = (unsigned char *)malloc(ctx.bound);
    ctx.compressed_len = 0;
    ctx.decompressed = (unsigned char *)malloc(ctx.size);

    printf("zlib %zu bytes\n", ctx.size);

    bench("Compress", (double)ctx.size, run_compress, &ctx);
    printf("Compressed: %zu -> %lu bytes\n", ctx.size, (unsigned long)ctx.compressed_len);
    bench("Decompress", (double)ctx.size, run_decompress, &ctx);

    if (memcmp(ctx.data, ctx.decompressed, ctx.size) != 0) {
        fprintf(stderr, "ERROR: decompressed data mismatch\n");
        return 1;
    }

    free(ctx.data);
    free(ctx.compressed);
    free(ctx.decompressed);
    return 0;
}
