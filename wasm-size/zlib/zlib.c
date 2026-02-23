#include <stdio.h>
#include <string.h>
#include "zlib-1.3.1/zlib.h"

// Test data: 1KB of patterned bytes
static unsigned char src[1024];

int main(void) {
    // Fill test data
    for (int i = 0; i < 1024; i++) {
        src[i] = (unsigned char)(i % 256);
    }

    // Compress
    unsigned char compressed[2048];
    uLongf compressed_len = sizeof(compressed);
    int rc = compress(compressed, &compressed_len, src, sizeof(src));
    if (rc != Z_OK) {
        printf("compress failed: %d\n", rc);
        return 1;
    }

    // Decompress
    unsigned char decompressed[1024];
    uLongf decompressed_len = sizeof(decompressed);
    rc = uncompress(decompressed, &decompressed_len, compressed, compressed_len);
    if (rc != Z_OK) {
        printf("uncompress failed: %d\n", rc);
        return 1;
    }

    // Verify
    if (decompressed_len != sizeof(src) || memcmp(src, decompressed, sizeof(src)) != 0) {
        printf("verification failed\n");
        return 1;
    }

    printf("zlib %s: %lu -> %lu -> %lu\n",
           zlibVersion(), (unsigned long)sizeof(src), (unsigned long)compressed_len,
           (unsigned long)decompressed_len);
    return 0;
}
