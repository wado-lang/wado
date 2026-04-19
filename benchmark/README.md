# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-04-17, wasmtime 43.0.0, gcc 13.3.0, rustc 1.94.1, Zig 0.15.2, Node.js v24.14.1, Linux x86_64.

## Prime Counting

Count primes up to 10M (integer arithmetic).

| Implementation    |     Time | vs best |
| ----------------- | -------: | ------: |
| C (gcc -O3)       | 3,282 ms |   1.00x |
| **Wado**          | 3,336 ms |   1.02x |
| JavaScript (Node) | 3,337 ms |   1.02x |

## Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation    |   Time | vs best |
| ----------------- | -----: | ------: |
| C (gcc -O3)       | 121 ms |   1.00x |
| **Wado**          | 139 ms |   1.15x |
| JavaScript (Node) | 140 ms |   1.16x |

## Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation    |  Time | vs best |
| ----------------- | ----: | ------: |
| C (gcc -O3)       | 46 ms |   1.00x |
| JavaScript (Node) | 68 ms |   1.48x |
| **Wado**          | 77 ms |   1.67x |

## Float-to-String

500K f64 conversions to fixed-point string.

| Implementation |  Time | vs best |
| -------------- | ----: | ------: |
| Zig (RelFast)  | 24 ms |   1.00x |
| Rust (native)  | 33 ms |   1.38x |
| **Wado**       | 47 ms |   1.96x |
| C (gcc -O3)    | 56 ms |   2.33x |

## Compression

zlib compress/decompress of twitter.json (631 KB) x 10 iterations.

| Implementation        | Compress | Decompress |  Total | vs best |
| --------------------- | -------: | ---------: | -----: | ------: |
| zlib-rs (Rust native) |    60 ms |      10 ms |  70 ms |   1.00x |
| C zlib (Wasm)         |   110 ms |      16 ms | 126 ms |   1.80x |
| **Wado** core:zlib    |   224 ms |     117 ms | 341 ms |   4.87x |

## JSON: twitter

Deserialize twitter.json (631 KB).

| Implementation           |    Time | vs best |
| ------------------------ | ------: | ------: |
| serde_json (Rust native) | 1.30 ms |   1.00x |
| JSON.parse (Node)        | 2.69 ms |   2.07x |
| **Wado** core:json       | 18.3 ms |  14.08x |

## JSON: canada

Deserialize canada.json (2.3 MB, geographic coordinates).

| Implementation           |     Time | vs best |
| ------------------------ | -------: | ------: |
| serde_json (Rust native) |  16.4 ms |   1.00x |
| JSON.parse (Node)        |  25.3 ms |   1.54x |
| **Wado** core:json       | 168.4 ms |  10.27x |

## JSON: catalog

Deserialize citm_catalog.json (1.7 MB, event catalog).

| Implementation           |    Time | vs best |
| ------------------------ | ------: | ------: |
| serde_json (Rust native) | 3.59 ms |   1.00x |
| JSON.parse (Node)        | 8.66 ms |   2.41x |
| **Wado** core:json       | 62.3 ms |  17.35x |

## SQL Parse

Parse 81 SQL statements (13 KB) x 100 iterations. Gale-generated parser vs sqlparser-rs.

| Implementation             |     Time | vs best |
| -------------------------- | -------: | ------: |
| sqlparser-rs (Rust native) |   188 ms |   1.00x |
| **Wado** (Gale)            | 1,227 ms |   6.53x |

## Syntax Highlight

Highlight 81 SQL statements (13 KB) x 100 iterations. Gale-generated highlighter vs tree-sitter.

| Implementation              |     Time | vs best |
| --------------------------- | -------: | ------: |
| tree-sitter (Rust native)   |   545 ms |   1.00x |
| tree-sitter (Wasm/wasmtime) |   694 ms |   1.27x |
| **Wado** (Gale)             | 5,121 ms |   9.39x |

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
