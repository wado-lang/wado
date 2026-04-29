# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-04-29, wasmtime 44.0.0, gcc 13.3.0, rustc 1.95.0, Zig 0.15.2, Node.js v24.14.1, Linux x86_64.

## Prime Counting

Count primes up to 10M (integer arithmetic).

| Implementation    |     Time | vs best |
| ----------------- | -------: | ------: |
| C (gcc -O3)       | 4,796 ms |   1.00x |
| **Wado**          | 5,359 ms |   1.12x |
| JavaScript (Node) | 6,437 ms |   1.34x |

## Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation    |   Time | vs best |
| ----------------- | -----: | ------: |
| JavaScript (Node) | 188 ms |   1.00x |
| **Wado**          | 189 ms |   1.01x |
| C (gcc -O3)       | 192 ms |   1.02x |

## Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation    |  Time | vs best |
| ----------------- | ----: | ------: |
| C (gcc -O3)       | 45 ms |   1.00x |
| JavaScript (Node) | 71 ms |   1.58x |
| **Wado**          | 95 ms |   2.11x |

## Float-to-String

500K f64 conversions to fixed-point string.

| Implementation |  Time | vs best |
| -------------- | ----: | ------: |
| Zig (RelFast)  | 34 ms |   1.00x |
| Rust (native)  | 49 ms |   1.44x |
| **Wado**       | 79 ms |   2.32x |
| C (gcc -O3)    | 86 ms |   2.53x |

## Compression

zlib compress/decompress of twitter.json (631 KB) x 10 iterations.

| Implementation        | Compress | Decompress |  Total | vs best |
| --------------------- | -------: | ---------: | -----: | ------: |
| zlib-rs (Rust native) |    42 ms |       5 ms |  47 ms |   1.00x |
| **Wado** core:zlib    |   293 ms |     168 ms | 461 ms |   9.81x |

## JSON: twitter

Deserialize twitter.json (631 KB).

| Implementation           |     Time | vs best |
| ------------------------ | -------: | ------: |
| serde_json (Rust native) |  1.03 ms |   1.00x |
| JSON.parse (Node)        |  2.50 ms |   2.43x |
| **Wado** core:json       | 17.01 ms |  16.51x |

## JSON: canada

Deserialize canada.json (2.3 MB, geographic coordinates).

| Implementation           |      Time | vs best |
| ------------------------ | --------: | ------: |
| serde_json (Rust native) |  11.29 ms |   1.00x |
| JSON.parse (Node)        |  17.39 ms |   1.54x |
| **Wado** core:json       | 149.97 ms |  13.28x |

## JSON: catalog

Deserialize citm_catalog.json (1.7 MB, event catalog).

| Implementation           |     Time | vs best |
| ------------------------ | -------: | ------: |
| serde_json (Rust native) |  2.97 ms |   1.00x |
| JSON.parse (Node)        |  8.93 ms |   3.01x |
| **Wado** core:json       | 55.37 ms |  18.64x |

## SQL Parse

Parse 81 SQL statements (13 KB) x 100 iterations. Gale-generated parser vs sqlparser-rs.

| Implementation             |     Time | vs best |
| -------------------------- | -------: | ------: |
| sqlparser-rs (Rust native) |   200 ms |   1.00x |
| **Wado** (Gale)            | 1,373 ms |   6.87x |

## Syntax Highlight

Highlight 81 SQL statements (13 KB) x 100 iterations. Gale-generated highlighter vs tree-sitter.

| Implementation            |     Time | vs best |
| ------------------------- | -------: | ------: |
| tree-sitter (Rust native) |   584 ms |   1.00x |
| **Wado** (Gale)           | 5,110 ms |   8.75x |

## Running

```sh
mise run benchmark-all              # run all

mise run benchmark-count-prime      # integer arithmetic
mise run benchmark-mandelbrot       # float arithmetic
mise run benchmark-sieve            # array operations
mise run benchmark-fts              # float-to-string
mise run benchmark-zlib             # compression
mise run benchmark-json-twitter     # JSON (631 KB)
mise run benchmark-json-canada      # JSON (2.3 MB)
mise run benchmark-json-catalog     # JSON (1.7 MB)
mise run benchmark-sqlite-parse     # SQL parsing
mise run benchmark-syntax-highlight # syntax highlighting
```

Prerequisites: `cc`, `cargo`, `zig`, `node` (managed by `mise install`).

## Profiling

```sh
# Guest profiling (all platforms) — view at https://profiler.firefox.com/
wado run --profile guest prog.wado
wado run --profile guest,output.json,5 prog.wado  # custom path, 5ms interval

# Linux perf
perf record -k mono wado run --profile jitdump prog.wado  # detailed (jitdump)
perf record -k mono wado run --profile perfmap prog.wado  # simple (perfmap)
samply record wado run --profile perfmap prog.wado         # samply
```
