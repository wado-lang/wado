# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-04-28, wasmtime 44.0.0, gcc 13.3.0, rustc 1.95.0, Zig 0.15.2, Node.js v24.14.1, Linux x86_64.

## Prime Counting

Count primes up to 10M (integer arithmetic).

| Implementation    |     Time | vs best |
| ----------------- | -------: | ------: |
| C (gcc -O3)       | 3,399 ms |   1.00x |
| **Wado**          | 3,442 ms |   1.01x |
| JavaScript (Node) | 3,822 ms |   1.12x |

## Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation    |   Time | vs best |
| ----------------- | -----: | ------: |
| C (gcc -O3)       | 153 ms |   1.00x |
| JavaScript (Node) | 161 ms |   1.05x |
| **Wado**          | 173 ms |   1.13x |

## Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation    |  Time | vs best |
| ----------------- | ----: | ------: |
| C (gcc -O3)       | 37 ms |   1.00x |
| JavaScript (Node) | 61 ms |   1.65x |
| **Wado**          | 76 ms |   2.05x |

## Float-to-String

500K f64 conversions to fixed-point string.

| Implementation |  Time | vs best |
| -------------- | ----: | ------: |
| Zig (RelFast)  | 26 ms |   1.00x |
| Rust (native)  | 35 ms |   1.35x |
| **Wado**       | 41 ms |   1.58x |
| C (gcc -O3)    | 55 ms |   2.12x |

## Compression

zlib compress/decompress of twitter.json (631 KB) x 10 iterations.

| Implementation        | Compress | Decompress |  Total | vs best |
| --------------------- | -------: | ---------: | -----: | ------: |
| zlib-rs (Rust native) |    30 ms |       3 ms |  33 ms |   1.00x |
| C zlib (Wasm)         |    78 ms |      11 ms |  89 ms |   2.70x |
| **Wado** core:zlib    |   201 ms |     101 ms | 302 ms |   9.15x |

## JSON: twitter

Deserialize twitter.json (631 KB).

| Implementation           |    Time | vs best |
| ------------------------ | ------: | ------: |
| serde_json (Rust native) | 0.63 ms |   1.00x |
| JSON.parse (Node)        | 1.49 ms |   2.36x |
| **Wado** core:json       | 12.3 ms |  19.51x |

## JSON: canada

Deserialize canada.json (2.3 MB, geographic coordinates).

| Implementation           |     Time | vs best |
| ------------------------ | -------: | ------: |
| serde_json (Rust native) |   8.2 ms |   1.00x |
| JSON.parse (Node)        |  10.3 ms |   1.25x |
| **Wado** core:json       | 105.4 ms |  12.84x |

## JSON: catalog

Deserialize citm_catalog.json (1.7 MB, event catalog).

| Implementation           |    Time | vs best |
| ------------------------ | ------: | ------: |
| serde_json (Rust native) | 2.64 ms |   1.00x |
| JSON.parse (Node)        | 3.92 ms |   1.48x |
| **Wado** core:json       | 37.1 ms |  14.05x |

## SQL Parse

Parse 81 SQL statements (13 KB) x 100 iterations. Gale-generated parser vs sqlparser-rs.

| Implementation             |     Time | vs best |
| -------------------------- | -------: | ------: |
| sqlparser-rs (Rust native) |   157 ms |   1.00x |
| **Wado** (Gale)            | 1,084 ms |   6.91x |

## Syntax Highlight

Highlight 81 SQL statements (13 KB) x 100 iterations. Gale-generated highlighter vs tree-sitter.

| Implementation              |     Time | vs best |
| --------------------------- | -------: | ------: |
| tree-sitter (Rust native)   |   468 ms |   1.00x |
| tree-sitter (Wasm/wasmtime) |   668 ms |   1.43x |
| **Wado** (Gale)             | 4,028 ms |   8.61x |

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
