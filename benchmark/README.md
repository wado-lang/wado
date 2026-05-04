# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-05-04, wasmtime 44.0.0, gcc 13.3.0, rustc 1.95.0, Zig 0.15.2, Node.js v24.14.1, Linux x86_64.

## Prime Counting

Count primes up to 10M (integer arithmetic).

| Implementation    |     Time | vs best |
| ----------------- | -------: | ------: |
| C (gcc -O3)       | 3,225 ms |   1.00x |
| **Wado**          | 3,260 ms |   1.01x |
| JavaScript (Node) | 3,313 ms |   1.03x |

## Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation    |   Time | vs best |
| ----------------- | -----: | ------: |
| **Wado**          | 194 ms |   1.00x |
| JavaScript (Node) | 196 ms |   1.01x |
| C (gcc -O3)       | 197 ms |   1.01x |

## Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation    |  Time | vs best |
| ----------------- | ----: | ------: |
| C (gcc -O3)       | 37 ms |   1.00x |
| JavaScript (Node) | 61 ms |   1.65x |
| **Wado**          | 73 ms |   1.97x |

## Float-to-String

500K f64 conversions to fixed-point string.

| Implementation |  Time | vs best |
| -------------- | ----: | ------: |
| Zig (RelFast)  | 30 ms |   1.00x |
| Rust (native)  | 37 ms |   1.23x |
| **Wado**       | 58 ms |   1.92x |
| C (gcc -O3)    | 68 ms |   2.27x |

## Compression

zlib compress/decompress of twitter.json (631 KB) x 10 iterations.

| Implementation        | Compress | Decompress |  Total | vs best |
| --------------------- | -------: | ---------: | -----: | ------: |
| zlib-rs (Rust native) |    29 ms |       4 ms |  33 ms |   1.00x |
| **Wado** core:zlib    |   201 ms |     112 ms | 313 ms |   9.59x |

## JSON: twitter

Deserialize twitter.json (631 KB).

| Implementation           |     Time | vs best |
| ------------------------ | -------: | ------: |
| serde_json (Rust native) |  0.78 ms |   1.00x |
| JSON.parse (Node)        |  1.85 ms |   2.37x |
| **Wado** core:json       | 14.13 ms |  18.12x |

## JSON: canada

Deserialize canada.json (2.3 MB, geographic coordinates).

| Implementation           |      Time | vs best |
| ------------------------ | --------: | ------: |
| serde_json (Rust native) |   8.60 ms |   1.00x |
| JSON.parse (Node)        |  12.12 ms |   1.41x |
| **Wado** core:json       | 123.58 ms |  14.37x |

## JSON: catalog

Deserialize citm_catalog.json (1.7 MB, event catalog).

| Implementation           |     Time | vs best |
| ------------------------ | -------: | ------: |
| serde_json (Rust native) |  2.41 ms |   1.00x |
| JSON.parse (Node)        |  4.71 ms |   1.96x |
| **Wado** core:json       | 46.91 ms |  19.51x |

## SQL Parse

Parse 81 SQL statements (13 KB) x 100 iterations. Gale-generated parser vs sqlparser-rs.

| Implementation             |   Time | vs best |
| -------------------------- | -----: | ------: |
| sqlparser-rs (Rust native) | 171 ms |   1.00x |
| **Wado** (Gale)            | 746 ms |   4.35x |

## Syntax Highlight

Highlight 81 SQL statements (13 KB) x 100 iterations. Gale-generated highlighter vs tree-sitter.

| Implementation            |     Time | vs best |
| ------------------------- | -------: | ------: |
| tree-sitter (Rust native) |   483 ms |   1.00x |
| **Wado** (Gale)           | 3,918 ms |   8.11x |

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
