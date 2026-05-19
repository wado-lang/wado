# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-05-19, wasmtime 44.0.0, gcc 13.3.0, rustc 1.95.0, Zig 0.15.2, Node.js v24.14.1, Linux x86_64.

## Prime Counting

Count primes up to 10M (integer arithmetic).

| Implementation    |     Time | vs best |
| ----------------- | -------: | ------: |
| C (gcc -O3)       | 3,279 ms |   1.00x |
| **Wado**          | 3,312 ms |   1.01x |
| JavaScript (Node) | 3,335 ms |   1.02x |

## Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation    |   Time | vs best |
| ----------------- | -----: | ------: |
| C (gcc -O3)       | 132 ms |   1.00x |
| **Wado**          | 139 ms |   1.05x |
| JavaScript (Node) | 141 ms |   1.07x |

## Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation    |  Time | vs best |
| ----------------- | ----: | ------: |
| C (gcc -O3)       | 58 ms |   1.00x |
| **Wado**          | 75 ms |   1.29x |
| JavaScript (Node) | 78 ms |   1.34x |

## Float-to-String

500K f64 conversions to fixed-point string.

| Implementation |  Time | vs best |
| -------------- | ----: | ------: |
| Zig (RelFast)  | 25 ms |   1.00x |
| Rust (native)  | 35 ms |   1.40x |
| **Wado**       | 45 ms |   1.78x |
| C (gcc -O3)    | 58 ms |   2.32x |

## Compression

zlib compress/decompress of twitter.json (631 KB) x 10 iterations.

| Implementation        | Compress | Decompress |  Total | vs best |
| --------------------- | -------: | ---------: | -----: | ------: |
| zlib-rs (Rust native) |    28 ms |       4 ms |  31 ms |   1.00x |
| **Wado** core:zlib    |   183 ms |      89 ms | 272 ms |   8.67x |

## JSON: twitter

Deserialize twitter.json (631 KB).

| Implementation           |     Time | vs best |
| ------------------------ | -------: | ------: |
| serde_json (Rust native) |  0.67 ms |   1.00x |
| JSON.parse (Node)        |  1.57 ms |   2.33x |
| **Wado** core:json       |  6.93 ms |  10.32x |

## JSON: canada

Deserialize canada.json (2.3 MB, geographic coordinates).

| Implementation           |      Time | vs best |
| ------------------------ | --------: | ------: |
| serde_json (Rust native) |   9.08 ms |   1.00x |
| JSON.parse (Node)        |  13.75 ms |   1.51x |
| **Wado** core:json       | 108.16 ms |  11.91x |

## JSON: catalog

Deserialize citm_catalog.json (1.7 MB, event catalog).

| Implementation             |     Time | vs best |
| -------------------------- | -------: | ------: |
| serde_json (Rust native)   |  2.14 ms |   1.00x |
| JSON.parse (Node)          |  4.33 ms |   2.02x |
| **Wado** v2 (hand-rolled¹) | 10.19 ms |   4.76x |
| **Wado** core:json         | 33.54 ms |  15.68x |

¹ `json_catalog/json_catalog_v2.wado` is a hand-rolled CitmCatalog parser
PoC (no `core:json` / `core:serde`). Kept as a marker of the upper bound
that's currently reachable without changes to `core:json`'s
sub-access-struct architecture. See its source for design notes.

## SQL Parse

Parse 81 SQL statements (13 KB) x 100 iterations. Gale-generated parser vs sqlparser-rs.

| Implementation             |   Time | vs best |
| -------------------------- | -----: | ------: |
| sqlparser-rs (Rust native) | 163 ms |   1.00x |
| **Wado** (Gale)            | 518 ms |   3.17x |

## Syntax Highlight

Highlight 81 SQL statements (13 KB) x 100 iterations. Gale-generated highlighter vs tree-sitter.

| Implementation            |     Time | vs best |
| ------------------------- | -------: | ------: |
| tree-sitter (Rust native) |   430 ms |   1.00x |
| **Wado** (Gale)           | 1,163 ms |   2.70x |

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
