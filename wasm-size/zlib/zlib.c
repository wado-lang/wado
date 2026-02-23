#include <stdio.h>
#include <string.h>
#include "zlib-1.3.1/zlib.h"

int main(void) {
    /* Read all gzip data from stdin */
    unsigned char input[8192];
    size_t input_len = 0;
    size_t n;
    while ((n = fread(input + input_len, 1, sizeof(input) - input_len, stdin)) > 0) {
        input_len += n;
    }

    /* Decompress gzip (16 + MAX_WBITS enables gzip format) */
    unsigned char output[8192];
    z_stream strm;
    memset(&strm, 0, sizeof(strm));
    strm.next_in = input;
    strm.avail_in = input_len;
    strm.next_out = output;
    strm.avail_out = sizeof(output);

    inflateInit2(&strm, 16 + MAX_WBITS);
    inflate(&strm, Z_FINISH);
    fwrite(output, 1, strm.total_out, stdout);
    inflateEnd(&strm);
    return 0;
}
